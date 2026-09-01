{-# LANGUAGE OverloadedStrings #-}

module Main (main) where

import Data.Aeson (encode)
import Data.ByteString qualified as BS
import Data.ByteString.Char8 qualified as BSC
import Data.ByteString.Lazy qualified as BL
import Data.ByteString.Lazy.Char8 qualified as BLC
import Data.Map.Strict qualified as Map
import Data.Text qualified as T
import Data.Time (UTCTime (..), fromGregorian)
import Recordings
import System.Directory
import System.FilePath
import System.IO.Temp (withSystemTempDirectory)
import Test.Syd

someTime :: UTCTime
someTime = UTCTime (fromGregorian 2026 8 31) 0

main :: IO ()
main = sydTest $ do
  describe "isAudioName" $ do
    it "accepts common audio extensions case-insensitively" $ do
      isAudioName "memo.m4a" `shouldBe` True
      isAudioName "MEMO.M4A" `shouldBe` True
      isAudioName "take.wav" `shouldBe` True
    it "rejects transcripts, hidden files, and iCloud placeholders" $ do
      isAudioName "memo.txt" `shouldBe` False
      isAudioName ".memo.m4a" `shouldBe` False
      isAudioName "memo.m4a.icloud" `shouldBe` False

  describe "readyFiles" $ do
    let ident1 = (100 :: Integer, someTime)
        ident2 = (200 :: Integer, someTime)
    it "keeps only files unchanged across both scans" $
      readyFiles
        (Map.fromList [("a", ident1), ("b", ident1), ("gone", ident1)])
        (Map.fromList [("a", ident1), ("b", ident2), ("new", ident1)])
        `shouldBe` ["a"]

  describe "encodeSafe" $ do
    it "flattens slashes injectively" $ do
      encodeSafe "a/b.m4a" `shouldBe` "a%2Fb.m4a"
      encodeSafe "50%.m4a" `shouldBe` "50%25.m4a"
      encodeSafe "a%2Fb" `shouldNotBe` encodeSafe "a/b"

  describe "tempName" $ do
    it "binds the spool name to path, size, and mtime" $ do
      let name = tempName "voice/memo.m4a" (12345, someTime)
      name `shouldSatisfy` notElem '/'
      T.pack name `shouldSatisfy` T.isInfixOf "12345"
      name `shouldSatisfy` (\n -> takeExtension n == ".txt")

  describe "targetStems" $
    it "starts with the stem and counts from -2" $
      take 3 (targetStems "memo") `shouldBe` ["memo", "memo-2", "memo-3"]

  describe "isInside" $ do
    it "detects containment and identity" $ do
      isInside "/a/b" "/a/b/c" `shouldBe` True
      isInside "/a/b" "/a/b" `shouldBe` True
      isInside "/a/b" "/a/bc" `shouldBe` False
      isInside "/a/b/c" "/a/b" `shouldBe` False

  describe "excerptEnds" $ do
    it "keeps short messages whole and both ends of long ones" $ do
      excerptEnds 10 "short" `shouldBe` "short"
      let long = replicate 50 'a' <> "CAUSE"
      excerptEnds 10 long `shouldSatisfy` T.isInfixOf "CAUSE" . T.pack
      excerptEnds 10 long `shouldSatisfy` T.isPrefixOf "aaaa" . T.pack

  describe "joinUrl" $
    it "joins regardless of trailing slash" $ do
      joinUrl "http://h/v1" "chat/completions" `shouldBe` "http://h/v1/chat/completions"
      joinUrl "http://h/v1/" "chat/completions" `shouldBe` "http://h/v1/chat/completions"

  describe "maxTokensFor" $ do
    it "clamps between 2048 and 32768" $ do
      maxTokensFor "short" `shouldBe` 2048
      maxTokensFor (T.replicate 100000 "x") `shouldBe` 32768
      maxTokensFor (T.replicate 10000 "x") `shouldBe` 5000

  describe "chatRequestValue" $ do
    let body = BL.toStrict (encode (chatRequestValue "glm" "fix it" "raw words"))
    it "disables streaming and thinking" $ do
      body `shouldSatisfy` BS.isInfixOf "\"stream\":false"
      body `shouldSatisfy` BS.isInfixOf "\"enable_thinking\":false"
    it "carries the prompt as system and transcript as user" $ do
      body `shouldSatisfy` BS.isInfixOf "\"system\""
      body `shouldSatisfy` BS.isInfixOf "\"fix it\""
      body `shouldSatisfy` BS.isInfixOf "\"raw words\""

  describe "decodeAsrText" $ do
    it "extracts the text field" $
      decodeAsrText "{\"text\":\"hello\",\"language\":\"en\"}" `shouldBe` Right "hello"
    it "fails on missing text" $
      decodeAsrText "{}" `shouldSatisfy` either (const True) (const False)

  describe "decodeChatText" $ do
    it "extracts the first choice's content" $
      decodeChatText
        "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"clean\"}}]}"
        `shouldBe` Right "clean"
    it "fails on empty choices" $
      decodeChatText "{\"choices\":[]}" `shouldSatisfy` either (const True) (const False)

  describe "writeFileDurable" $
    it "writes atomically and leaves no temp file" $
      withSystemTempDirectory "rec" $ \dir -> do
        let p = dir </> "out.txt"
        writeFileDurable p "content\n"
        readFile p >>= (`shouldBe` "content\n")
        doesFileExist (p <.> "tmp") >>= (`shouldBe` False)

  describe "movePath" $
    it "moves content and removes the source" $
      withSystemTempDirectory "rec" $ \dir -> do
        let src = dir </> "a.m4a"
            dst = dir </> "b.m4a"
        BSC.writeFile src "audio-bytes"
        movePath src dst
        BSC.readFile dst >>= (`shouldBe` "audio-bytes")
        doesFileExist src >>= (`shouldBe` False)

  describe "freshTargets" $ do
    it "uses the plain stem when free" $
      withSystemTempDirectory "rec" $ \dir -> do
        (txt, arc) <- freshTargets dir dir (dir </> "memo.m4a")
        txt `shouldBe` dir </> "memo.txt"
        arc `shouldBe` dir </> "memo.m4a"
    it "moves to -2 when either destination is taken" $
      withSystemTempDirectory "rec" $ \dir -> do
        writeFile (dir </> "memo.txt") "occupied"
        (txt, arc) <- freshTargets dir dir (dir </> "memo.m4a")
        txt `shouldBe` dir </> "memo-2.txt"
        arc `shouldBe` dir </> "memo-2.m4a"

  describe "pruneEmptyDirs" $
    it "removes empty and .DS_Store-only subdirectories but never the root" $
      withSystemTempDirectory "rec" $ \dir -> do
        createDirectoryIfMissing True (dir </> "empty1" </> "empty2")
        createDirectoryIfMissing True (dir </> "junk")
        createDirectoryIfMissing True (dir </> "keep")
        writeFile (dir </> "junk" </> ".DS_Store") ""
        writeFile (dir </> "keep" </> "file.m4a") "x"
        pruneEmptyDirs dir
        doesDirectoryExist (dir </> "empty1") >>= (`shouldBe` False)
        doesDirectoryExist (dir </> "junk") >>= (`shouldBe` False)
        doesDirectoryExist (dir </> "keep") >>= (`shouldBe` True)
        doesDirectoryExist dir >>= (`shouldBe` True)

  describe "decode fixtures stay lazy-bytestring friendly" $
    it "accepts Char8-packed JSON" $
      decodeAsrText (BLC.pack "{\"text\":\"ok\"}") `shouldBe` Right "ok"
