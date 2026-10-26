# marina demo. Regenerate with:  demo/record.sh   (requires: vhs)
# Records `marina demo` — made-up servers and agent sessions — so the GIF is
# the same on any machine and never shows a real one. Run via record.sh, not
# directly: it builds --release and sets $MARINA_BIN.
Output marina.gif

Set Shell "bash"
Set FontSize 16
Set Width 1240
Set Height 820
Set Padding 18
Set TypingSpeed 55ms
Set WindowBar Colorful

# --- off-camera: launch the demo ---
Hide
Type "clear && $MARINA_BIN demo"
Enter
Sleep 2s
Show

# settle on the populated cockpit
Sleep 2500ms

# fold and unfold the checkout project (2 servers)
Enter
Sleep 1200ms
Enter
Sleep 1200ms

# a server's log, inline; q closes the pane
Down
Sleep 600ms
Type "t"
Sleep 3s
Type "q"
Sleep 1s

# sort by cpu, then mem, then back to port
Type "s"
Sleep 1200ms
Type "s"
Sleep 1200ms
Type "s"
Sleep 1s

# filter to what Claude Code sessions started; Esc puts the old filter back
Type "/claude"
Sleep 2s
Escape
Sleep 1s

# the server an ended session left behind: X stops it, the row stays put
Down
Sleep 500ms
Down
Sleep 900ms
Type "X"
Sleep 2500ms

# X on a group asks first; any other key cancels
Type "g"
Sleep 800ms
Type "X"
Sleep 2500ms
Escape
Sleep 1200ms

# group by who started them: agent sessions with their servers
Type "v"
Sleep 1500ms
Down
Sleep 2500ms
Type "v"
Sleep 1s

# every key
Type "?"
Sleep 3s
Escape
Sleep 800ms

# back to the top — leave the full cockpit on screen
Type "g"
Sleep 2s
