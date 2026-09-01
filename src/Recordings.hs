{-# LANGUAGE LambdaCase #-}
{-# LANGUAGE OverloadedStrings #-}
{-# LANGUAGE ScopedTypeVariables #-}

{- | Watch a directory tree for audio recordings; transcribe, clean, and
archive each one.

The design premise that keeps this small: the audio file is the only
source of truth, and every other artifact can be recomputed from it.
Data safety therefore reduces to one rule — the audio file is never
moved out of the watch tree until its cleaned transcript has been
durably published — plus atomic (write-to-temp, fsync, rename)
publication of every file we create.  A crash at any point leaves
either the audio still in place (the sweep redoes the work, reusing
the raw transcript when the file is unchanged) or the work complete.
-}
module Recordings (
  -- * Options
  Options (..),
  Transcriber (..),
  optionsInfo,

  -- * Entry point
  run,

  -- * Scanning (pure core)
  Identity,
  isAudioName,
  readyFiles,
  encodeSafe,
  tempName,
  targetStems,
  isInside,
  excerptEnds,

  -- * HTTP (pure core)
  joinUrl,
  maxTokensFor,
  chatRequestValue,
  decodeAsrText,
  decodeChatText,

  -- * Filesystem helpers
  writeFileDurable,
  movePath,
  freshTargets,
  pruneEmptyDirs,
)
where

import Control.Concurrent (threadDelay)
import Control.Exception (
  AsyncException,
  IOException,
  SomeException,
  catch,
  finally,
  fromException,
  throwIO,
  try,
 )
import Control.Monad (foldM, forM, forM_, unless, void, when)
import Data.Aeson (Value, eitherDecode, encode, object, withObject, (.:), (.=))
import Data.Aeson.Types qualified as Aeson
import Data.ByteString.Char8 qualified as BS
import Data.ByteString.Lazy qualified as BL
import Data.Char (toLower)
import Data.List (isPrefixOf)
import Data.Map.Strict (Map)
import Data.Map.Strict qualified as Map
import Data.Text (Text)
import Data.Text qualified as T
import Data.Text.Encoding qualified as TE
import Data.Text.IO qualified as TIO
import Data.Time (defaultTimeLocale, formatTime, getZonedTime)
import Data.Time.Clock (UTCTime)
import Data.Time.Clock.POSIX (utcTimeToPOSIXSeconds)
import Data.X509.CertificateStore (CertificateStore, readCertificateStore)
import GHC.IO.Encoding (setLocaleEncoding)
import GHC.IO.Handle.Lock (LockMode (..), hTryLock)
import Network.Connection (TLSSettings (..))
import Network.HTTP.Client
import Network.HTTP.Client.MultipartFormData (formDataBody, partBS, partFileSource)
import Network.HTTP.Client.TLS (mkManagerSettings, newTlsManager)
import Network.HTTP.Types.Status (statusCode)
import Network.TLS (clientShared, clientSupported, defaultParamsClient, sharedCAStore, supportedCiphers)
import Network.TLS.Extra.Cipher (ciphersuite_default)
import Options.Applicative
import System.Directory
import System.Environment (lookupEnv)
import System.Exit (ExitCode (..), exitFailure)
import System.FilePath
import System.IO
import System.Posix.IO (closeFd, handleToFd)
import System.Posix.Unistd (fileSynchronise)
import System.Process (readProcessWithExitCode)

-- * Options

data Transcriber
  = {- | OpenAI-compatible @/audio/transcriptions@ endpoint: base URL and
    model name.
    -}
    AsrHttp String String
  | {- | Fallback: a local command (given as words) run with the path of a
    16 kHz mono WAV conversion of the recording appended; its stdout is
    the transcript.
    -}
    AsrShell String
  deriving (Eq, Show)

data Options = Options
  { optWatchDir :: FilePath
  , optOutputDir :: FilePath
  , optArchiveDir :: FilePath
  , optPromptFile :: FilePath
  , optTranscriber :: Transcriber
  , optLlmUrl :: String
  , optLlmModel :: String
  , optApiKey :: String
  , optLanguage :: String
  , optPollSecs :: Int
  , optOnce :: Bool
  }
  deriving (Eq, Show)

optionsParser :: Parser Options
optionsParser =
  Options
    <$> argument str (metavar "WATCH_DIR" <> help "Directory tree to watch for audio files")
    <*> strOption (long "output" <> metavar "DIR" <> help "Directory receiving cleaned .txt transcripts")
    <*> strOption (long "archive" <> metavar "DIR" <> help "Directory receiving processed audio files")
    <*> strOption (long "prompt" <> metavar "FILE" <> help "Markdown file holding the fix-transcription prompt")
    <*> transcriber
    <*> strOption (long "llm-url" <> metavar "URL" <> help "OpenAI-compatible base URL for cleanup (e.g. http://localhost:8000/v1)")
    <*> strOption (long "llm-model" <> metavar "NAME" <> help "Model name for transcript cleanup")
    <*> strOption (long "api-key" <> metavar "KEY" <> value "dummy-key" <> showDefault <> help "Bearer token for both endpoints")
    <*> strOption (long "language" <> metavar "CODE" <> value "en" <> showDefault <> help "ISO 639-1 language for transcription")
    <*> option auto (long "poll" <> metavar "SECONDS" <> value 30 <> showDefault <> help "Seconds between sweeps")
    <*> switch (long "once" <> help "Run a single sweep and exit")
 where
  transcriber =
    AsrHttp
      <$> strOption (long "asr-url" <> metavar "URL" <> help "OpenAI-compatible base URL for transcription (e.g. https://hera.lan:8443/v1)")
      <*> strOption (long "asr-model" <> metavar "NAME" <> help "Transcription model name (e.g. cohere-transcribe-03-2026-mlx-fp16)")
        <|> AsrShell
      <$> strOption (long "asr-command" <> metavar "CMD" <> help "Local transcription command; run with the path of a 16 kHz mono WAV conversion appended, stdout is the transcript")

optionsInfo :: ParserInfo Options
optionsInfo =
  info
    (optionsParser <**> helper)
    ( fullDesc
        <> progDesc "Transcribe audio recordings as they arrive, clean the transcripts with an LLM, and archive the audio"
    )

-- * Scanning: pure core

{- | A cheap notion of file identity: size and modification time.  A file
is only processed when its identity is unchanged across two scans a
couple of seconds apart, so half-synced uploads are left alone.
-}
type Identity = (Integer, UTCTime)

audioExtensions :: [String]
audioExtensions = [".m4a", ".mp3", ".wav", ".aac", ".flac", ".ogg", ".opus", ".aiff", ".aif"]

isAudioName :: FilePath -> Bool
isAudioName name =
  not ("." `isPrefixOf` name)
    && map toLower (takeExtension name) `elem` audioExtensions

-- | Files present in both scans with an unchanged identity, in stable order.
readyFiles :: Map FilePath Identity -> Map FilePath Identity -> [FilePath]
readyFiles before after =
  Map.keys (Map.filterWithKey (\p i -> Map.lookup p before == Just i) after)

{- | Encode a watch-relative path into a single flat filename component,
injectively — the paths @a\/b@ and @a_b@ stay distinct.
-}
encodeSafe :: FilePath -> FilePath
encodeSafe = concatMap $ \case
  '/' -> "%2F"
  '%' -> "%25"
  c -> [c]

{- | Name of the raw-transcript spool file for one audio file at one
identity.  A crash between transcription and publication leaves this
behind; the next sweep reuses it instead of repeating the expensive
transcription, and a changed identity simply misses the cache.
-}
tempName :: FilePath -> Identity -> FilePath
tempName rel (size, mtime) =
  encodeSafe rel
    <> "."
    <> show size
    <> "."
    <> show (floor (utcTimeToPOSIXSeconds mtime) :: Integer)
    <> ".raw.txt"

-- | Candidate basename stems for publication: @x@, @x-2@, @x-3@, ...
targetStems :: String -> [String]
targetStems stem = stem : [stem <> "-" <> show n | n <- [(2 :: Int) ..]]

{- | Both ends of a long message: a rendered HttpException buries its
cause after a large Request dump, so the tail matters most.
-}
excerptEnds :: Int -> String -> String
excerptEnds n s
  | length s <= 2 * n + 3 = s
  | otherwise = take n s <> " … " <> drop (length s - n) s

{- | Is @child@ equal to or beneath @parent@?  Both must be absolute and
normalised.
-}
isInside :: FilePath -> FilePath -> Bool
isInside parent child =
  splitDirectories (dropTrailingPathSeparator parent)
    `isPrefixOf` splitDirectories (dropTrailingPathSeparator child)

-- * HTTP: pure core

joinUrl :: String -> String -> String
joinUrl base path = reverse (dropWhile (== '/') (reverse base)) <> "/" <> path

{- | Cleanup output is about the size of its input (~4 chars/token), so a
cap of half the character count is roughly double the expected tokens;
bounded to keep runaway generation finite.
-}
maxTokensFor :: Text -> Int
maxTokensFor t = min 32768 (max 2048 (T.length t `div` 2))

chatRequestValue :: String -> Text -> Text -> Value
chatRequestValue model prompt transcript =
  object
    [ "model" .= model
    , "messages"
        .= [ object ["role" .= ("system" :: Text), "content" .= prompt]
           , object ["role" .= ("user" :: Text), "content" .= transcript]
           ]
    , "temperature" .= (0.3 :: Double)
    , "max_tokens" .= maxTokensFor transcript
    , "stream" .= False
    , -- Thinking models served by omlx stream reasoning into the content
      -- and burn the token cap; cleanup needs only the answer.
      "chat_template_kwargs" .= object ["enable_thinking" .= False]
    ]

decodeAsrText :: BL.ByteString -> Either String Text
decodeAsrText body = do
  v <- eitherDecode body
  Aeson.parseEither (withObject "transcription" (.: "text")) v

decodeChatText :: BL.ByteString -> Either String Text
decodeChatText body = do
  v <- eitherDecode body
  flip Aeson.parseEither v $
    withObject "completion" $ \o ->
      o .: "choices" >>= \case
        [] -> fail "no choices in completion"
        (c : _) -> c .: "message" >>= (.: "content")

-- * Filesystem helpers

{- | Write content to a sibling temp file, fsync it, and atomically rename
into place.  ponytail: plain fsync, not F_FULLFSYNC; on a journaled
filesystem this covers crash-safety short of power loss mid-barrier.
-}
writeFileDurable :: FilePath -> Text -> IO ()
writeFileDurable path content = do
  let tmp = path <.> "tmp"
  h <- openBinaryFile tmp WriteMode
  BS.hPut h (TE.encodeUtf8 content)
  hFlush h
  fd <- handleToFd h
  fileSynchronise fd
  closeFd fd
  renameFile tmp path

{- | Move a file, falling back to copy-verify-delete when rename fails
(e.g. across filesystems).  The source is only removed once the
destination has the same size.
-}
movePath :: FilePath -> FilePath -> IO ()
movePath src dst =
  renameFile src dst `catch` \(_ :: IOException) -> do
    copyFileWithMetadata src dst
    srcSize <- getFileSize src
    dstSize <- getFileSize dst
    unless (srcSize == dstSize) $
      ioError (userError ("copy of " <> src <> " to " <> dst <> " has wrong size"))
    removeFile src

{- | First stem whose transcript and archive destinations are both free.
ponytail: existence-check then create; safe because this daemon is the
only writer (it holds a lock), so no uniquification races exist.
-}
freshTargets :: FilePath -> FilePath -> FilePath -> IO (FilePath, FilePath)
freshTargets outputDir archiveDir audio = go (targetStems (takeBaseName audio))
 where
  ext = takeExtension audio
  go [] = ioError (userError "exhausted target stems")
  go (s : ss) = do
    let txt = outputDir </> s <.> "txt"
        arc = archiveDir </> s <> ext
    taken <- (||) <$> doesPathExist txt <*> doesPathExist arc
    if taken then go ss else pure (txt, arc)

{- | Remove subdirectories (never the root) that are empty except for
.DS_Store droppings.  Hidden directories and symlinks are left alone.
-}
pruneEmptyDirs :: FilePath -> IO ()
pruneEmptyDirs root = void (go root True)
 where
  go dir isRoot = do
    entries <- listDirectory dir `catch` \(_ :: IOException) -> pure []
    remaining <- fmap concat . forM entries $ \e -> do
      let p = dir </> e
      sym <- pathIsSymbolicLink p `catch` \(_ :: IOException) -> pure True
      isDir <- if sym then pure False else doesDirectoryExist p
      if isDir && not ("." `isPrefixOf` e)
        then do
          emptied <- go p False
          pure [e | not emptied]
        else pure [e]
    if not isRoot && all (== ".DS_Store") remaining
      then do
        forM_ remaining $ \e ->
          removeFile (dir </> e) `catch` \(_ :: IOException) -> pure ()
        (True <$ removeDirectory dir) `catch` \(_ :: IOException) -> pure False
      else pure False

-- * Runtime environment

data Env = Env
  { envOpts :: Options
  , envStateDir :: FilePath
  , envAsrManager :: Manager
  , envLlmManager :: Manager
  }

logMsg :: String -> IO ()
logMsg msg = do
  now <- getZonedTime
  putStrLn (formatTime defaultTimeLocale "%FT%T%z" now <> " " <> msg)

httpTimeout :: Int -> Request -> Request
httpTimeout secs req = req{responseTimeout = responseTimeoutMicro (secs * 1000000)}

authorize :: String -> Request -> Request
authorize key req =
  req{requestHeaders = ("Authorization", "Bearer " <> BS.pack key) : requestHeaders req}

{- | The CA bundle named by SSL_CERT_FILE (the omlx proxy's private CA),
validated against exactly like Python's
@create_default_context(cafile=...)@.
-}
loadCaStore :: IO (Maybe CertificateStore)
loadCaStore =
  lookupEnv "SSL_CERT_FILE" >>= \case
    Nothing -> pure Nothing
    Just caFile ->
      readCertificateStore caFile >>= \case
        Nothing -> ioError (userError ("SSL_CERT_FILE has no usable certificates: " <> caFile))
        Just store -> pure (Just store)

{- | A manager for one endpoint.  TLS client parameters are fixed per
manager, so with a custom CA each endpoint gets its own manager whose
parameters name that endpoint's host — keeping hostname verification
intact alongside the private trust store.
-}
newManagerFor :: Maybe CertificateStore -> String -> IO Manager
newManagerFor Nothing _ = newTlsManager
newManagerFor (Just store) url = do
  req <- parseRequest url
  let base = defaultParamsClient (BS.unpack (host req)) (BS.pack (show (port req)))
      params =
        base
          { clientShared = (clientShared base){sharedCAStore = store}
          , clientSupported = (clientSupported base){supportedCiphers = ciphersuite_default}
          }
  newManager (mkManagerSettings (TLSSettings params) Nothing)

expectOk :: String -> Response BL.ByteString -> IO BL.ByteString
expectOk what resp = do
  let code = statusCode (responseStatus resp)
  unless (code >= 200 && code < 300) $
    ioError . userError $
      what <> " failed with HTTP " <> show code <> ": " <> excerpt (responseBody resp)
  pure (responseBody resp)
 where
  excerpt = T.unpack . T.take 300 . TE.decodeUtf8Lenient . BL.toStrict

-- * Pipeline steps

{- | Transcribe one audio file to raw text.  The audio is first converted
to 16 kHz mono WAV — the model's native input — with macOS-native
afconvert, so neither the server nor a local command ever needs its own
m4a decoder.
-}
runAsr :: Env -> FilePath -> IO Text
runAsr env audio = do
  let wav = envStateDir env </> "convert.wav"
  removeQuietly wav
  (code, _, err) <-
    readProcessWithExitCode
      "afconvert"
      ["-f", "WAVE", "-d", "LEI16@16000", "-c", "1", audio, wav]
      ""
  unless (code == ExitSuccess) $
    ioError (userError ("afconvert failed on " <> audio <> ": " <> take 300 err))
  transcribeWav env wav `finally` removeQuietly wav

transcribeWav :: Env -> FilePath -> IO Text
transcribeWav env wav = case optTranscriber (envOpts env) of
  AsrShell cmdline -> case words cmdline of
    [] -> ioError (userError "--asr-command is empty")
    (cmd : args) -> do
      (code, out, err) <- readProcessWithExitCode cmd (args ++ [wav]) ""
      case code of
        ExitSuccess -> pure (T.pack out)
        ExitFailure n ->
          ioError . userError $
            cmd <> " exited " <> show n <> ": " <> take 300 err
  AsrHttp base model -> do
    req0 <- parseRequest (joinUrl base "audio/transcriptions")
    req <-
      formDataBody
        [ partBS "model" (BS.pack model)
        , partBS "language" (BS.pack (optLanguage (envOpts env)))
        , partFileSource "file" wav
        ]
        req0
    resp <-
      httpLbs
        (httpTimeout 3600 (authorize (optApiKey (envOpts env)) req))
        (envAsrManager env)
    body <- expectOk "transcription" resp
    either (ioError . userError . ("bad transcription response: " <>)) pure $
      decodeAsrText body

-- | Clean a raw transcript through the LLM with the prompt file's contents.
runLlm :: Env -> Text -> IO Text
runLlm env transcript = do
  let opts = envOpts env
  prompt <- TIO.readFile (optPromptFile opts)
  req0 <- parseRequest (joinUrl (optLlmUrl opts) "chat/completions")
  let req =
        req0
          { method = "POST"
          , requestHeaders = [("Content-Type", "application/json")]
          , requestBody =
              RequestBodyLBS (encode (chatRequestValue (optLlmModel opts) (T.strip prompt) transcript))
          }
  resp <-
    httpLbs (httpTimeout 600 (authorize (optApiKey opts) req)) (envLlmManager env)
  body <- expectOk "cleanup" resp
  either (ioError . userError . ("bad completion response: " <>)) pure $
    decodeChatText body

fileIdentity :: FilePath -> IO (Maybe Identity)
fileIdentity p =
  (Just <$> ((,) <$> getFileSize p <*> getModificationTime p))
    `catch` \(_ :: IOException) -> pure Nothing

removeQuietly :: FilePath -> IO ()
removeQuietly p = removeFile p `catch` \(_ :: IOException) -> pure ()

blank :: Text -> Bool
blank = T.null . T.strip

{- | The whole per-file pipeline.  Ordering is the crash-safety argument:

1. raw transcript is spooled durably before cleanup, so a crash never
   repeats the expensive transcription for an unchanged file;
2. the cleaned transcript is published durably before the audio is
   touched, so the audio cannot outrun its transcript;
3. only then is the audio moved to the archive and the spool removed.

ponytail: a crash exactly between 2 and 3 republishes under "name-2" on
the next sweep — a visible duplicate, never a loss.
-}
processFile :: Env -> FilePath -> Identity -> IO ()
processFile env audio ident = do
  let opts = envOpts env
      rel = makeRelative (optWatchDir opts) audio
      spool = envStateDir env </> tempName rel ident
  raw <-
    doesFileExist spool >>= \case
      True -> do
        logMsg ("reusing spooled transcript for " <> rel)
        TIO.readFile spool
      False -> do
        logMsg ("transcribing " <> rel)
        t <- runAsr env audio
        when (blank t) $ ioError (userError ("empty transcription for " <> rel))
        writeFileDurable spool t
        pure t
  logMsg ("cleaning " <> rel)
  cleaned <- runLlm env raw
  when (blank cleaned) $ ioError (userError ("empty cleanup result for " <> rel))
  fileIdentity audio >>= \now ->
    unless (now == Just ident) $
      ioError (userError (rel <> " changed while being processed"))
  (txt, arc) <- freshTargets (optOutputDir opts) (optArchiveDir opts) audio
  writeFileDurable txt (T.stripEnd cleaned <> "\n")
  movePath audio arc
  removeQuietly spool
  logMsg ("published " <> takeFileName txt <> "; archived " <> takeFileName arc)

-- * Sweep loop

maxAttempts :: Int
maxAttempts = 5

type Fails = Map FilePath (Identity, Int)

scanAudio :: FilePath -> IO (Map FilePath Identity)
scanAudio root = do
  files <- walk root
  Map.fromList . concat <$> forM files (\p -> maybe [] (\i -> [(p, i)]) <$> fileIdentity p)
 where
  walk dir = do
    entries <- listDirectory dir `catch` \(_ :: IOException) -> pure []
    fmap concat . forM entries $ \e ->
      if "." `isPrefixOf` e
        then pure []
        else do
          let p = dir </> e
          isDir <- doesDirectoryExist p
          if isDir
            then walk p
            else pure [p | isAudioName e]

stepFile :: Env -> Map FilePath Identity -> Fails -> FilePath -> IO Fails
stepFile env scan fails audio = case Map.lookup audio scan of
  Nothing -> pure fails
  Just ident
    | Just (i, n) <- Map.lookup audio fails
    , i == ident
    , n >= maxAttempts ->
        pure fails
    | otherwise ->
        try (processFile env audio ident) >>= \case
          Right () -> pure (Map.delete audio fails)
          Left (e :: SomeException) -> do
            forM_ (fromException e) $ \(ae :: AsyncException) -> throwIO ae
            let n' = case Map.lookup audio fails of
                  Just (i, n) | i == ident -> n + 1
                  _ -> 1
            logMsg ("ERROR processing " <> audio <> ": " <> excerptEnds 400 (unwords (words (show e))))
            when (n' >= maxAttempts) $
              logMsg ("giving up on " <> audio <> " until it changes (" <> show n' <> " attempts)")
            pure (Map.insert audio (ident, n') fails)

sweep :: Env -> Fails -> IO Fails
sweep env fails = do
  let watch = optWatchDir (envOpts env)
  before <- scanAudio watch
  unless (Map.null before) $ threadDelay 2000000 -- settle half-written files
  after <- scanAudio watch
  fails' <- foldM (stepFile env after) fails (readyFiles before after)
  pruneEmptyDirs watch
  pure (Map.restrictKeys fails' (Map.keysSet after))

run :: Options -> IO ()
run opts0 = do
  setLocaleEncoding utf8
  hSetBuffering stdout LineBuffering
  hSetBuffering stderr LineBuffering
  watch <- makeAbsolute (optWatchDir opts0)
  output <- makeAbsolute (optOutputDir opts0)
  archive <- makeAbsolute (optArchiveDir opts0)
  stateDir <- getXdgDirectory XdgState "recordings"
  createDirectoryIfMissing True stateDir
  createDirectoryIfMissing True output
  createDirectoryIfMissing True archive
  forM_ [("--archive", archive), ("state", stateDir)] $ \(what, dir) ->
    when (isInside watch dir) $ do
      logMsg ("ERROR " <> what <> " directory " <> dir <> " lies inside the watch tree " <> watch)
      exitFailure
  promptExists <- doesFileExist (optPromptFile opts0)
  unless promptExists $
    logMsg ("WARN prompt file missing (will fail per file until it appears): " <> optPromptFile opts0)
  -- A single flock guards against a stray second copy double-processing;
  -- launchd already keeps just one service instance.
  lockH <- openFile (stateDir </> "lock") WriteMode
  gotLock <- hTryLock lockH ExclusiveLock
  unless gotLock $ do
    logMsg "ERROR another instance holds the lock; exiting"
    exitFailure
  caStore <- loadCaStore
  asrMgr <- case optTranscriber opts0 of
    AsrHttp base _ -> newManagerFor caStore base
    AsrShell _ -> newManagerFor Nothing ""
  llmMgr <- newManagerFor caStore (optLlmUrl opts0)
  let opts = opts0{optWatchDir = watch, optOutputDir = output, optArchiveDir = archive}
      env = Env{envOpts = opts, envStateDir = stateDir, envAsrManager = asrMgr, envLlmManager = llmMgr}
  logMsg ("watching " <> watch <> " -> " <> output <> " (archive " <> archive <> ")")
  let loop fails = do
        fails' <-
          sweep env fails `catch` \(e :: IOException) -> do
            logMsg ("ERROR sweep failed: " <> show e)
            pure fails
        unless (optOnce opts) $ do
          threadDelay (max 1 (optPollSecs opts) * 1000000)
          loop fails'
  loop Map.empty
