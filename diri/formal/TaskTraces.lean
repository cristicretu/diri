import Reliability
open Diri

def commands := [(TaskEvent.acknowledge, "ack"), (.complete, "complete"), (.cancel, "cancel")]

def main : IO Unit := do
  for (a, aname) in commands do
    for (b, bname) in commands do
      for (c, cname) in commands do
        let result := [TaskEvent.submit, a, .appRestart, b, .engineRestart, c].foldl taskStep {}
        IO.println (String.intercalate "\t" [aname, bname, cname,
          toString result.acknowledged, toString result.terminal])
