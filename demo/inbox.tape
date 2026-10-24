# The agent inbox, from `marina demo` (made-up sessions; nothing real is
# touched). Regenerate with:  demo/record.sh   (requires: vhs)
# The scenario runs on the clock, so the sleeps below line up with it: web's
# tests finish 12s in, mobile stops to ask 17s in. While keys are being
# pressed the inbox holds its order (8s), so moving by row is predictable.
Output inbox.gif

Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 820
Set Padding 18
Set TypingSpeed 60ms
Set WindowBar Colorful

Hide
Type "clear && $MARINA_BIN demo"
Enter
Sleep 1s
Show

# three sessions need you: answer codex's question without leaving marina
Sleep 2500ms
Type "j"
Sleep 1200ms
Type "r"
Sleep 500ms
Type "per API key"
Sleep 700ms
Enter
Sleep 1500ms

# approve the migration the first one is waiting on
Type "k"
Sleep 1200ms
Type "y"
Sleep 2000ms

# a session in a terminal marina can't type into says so
Type "jj"
Sleep 3000ms

# hands off: web finishes, mobile stops to ask, the inbox settles
Sleep 7000ms
Type "g"
Sleep 2000ms

# read its screen in place
Type "p"
Sleep 3000ms
Type "p"
Sleep 800ms

# the servers they started, with the runaway one flagged
Type "a"
Sleep 4000ms
