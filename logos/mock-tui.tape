Output logos/mock-tui.mp4

Require ffmpeg
Require ttyd

Set Shell "bash"
Set Theme "Catppuccin Mocha"
Set FontSize 16
Set Width 1280
Set Height 720
Set Padding 12
Set Framerate 15

Hide
Type "export SIRBONE_RECORDING=1"
Enter
Type "exec target/debug/examples/mock_tui"
Enter
Show
Sleep 13s
