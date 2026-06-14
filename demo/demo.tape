# marina demo. Regenerate with:  demo/record.sh   (requires: vhs, node)
# Run via record.sh, not directly: it builds --release and runs vhs from a
# neutral cwd (so VHS's own helper processes aren't picked up as targets) with
# $MARINA_BIN / $MARINA_SCENE set.
Output marina.gif

Set Shell "bash"
Set FontSize 16
Set Width 1240
Set Height 820
Set Padding 18
Set TypingSpeed 55ms
Set WindowBar Colorful

# --- off-camera: start a demo scene, then launch marina ---
Hide
Type "bash $MARINA_SCENE/scene-up.sh"
Enter
Sleep 3s
Type "clear"
Enter
Type "$MARINA_BIN"
Enter
Sleep 3s
Show

# settle on the populated cockpit
Sleep 2s

# grouping: fold then unfold the client-portal project (2 services)
Enter
Sleep 1500ms
Enter
Sleep 1500ms

# tail a service's logs inline
Down
Sleep 600ms
Type "T"
Sleep 3s
Escape
Sleep 1s

# cycle the sort: port -> cpu -> mem -> port
Type "s"
Sleep 1300ms
Type "s"
Sleep 1300ms
Type "s"
Sleep 1s

# live filter, then clear
Type "/client"
Sleep 2s
Escape
Sleep 1s

# the help overlay
Type "?"
Sleep 2500ms
Escape
Sleep 800ms

# back to the top — leave the full cockpit on screen
Type "g"
Sleep 2000ms
