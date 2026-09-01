module Main (main) where

import Options.Applicative (execParser)
import Recordings (optionsInfo, run)

main :: IO ()
main = run =<< execParser optionsInfo
