# VHS tape for the arca TUI demo GIF. Drives the `arca` client against a daemon
# serving a freshly-seeded, entirely fictional database (see demo/record.sh).
# Record with:  sh demo/record.sh   (which sets PATH + starts the demo daemon)
#
# https://github.com/charmbracelet/vhs

Output demo/arca.gif

Require arca

Set Shell "bash"
Set FontSize 14
Set Width 1400
Set Height 860
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms
# 24fps keeps the GIF small for a README without visible choppiness.
Set Framerate 24

# Connect to the demo daemon's read socket (read-only views — no mutations).
Hide
Type "arca --socket /tmp/arca-demo/read.sock"
Enter
Sleep 2.5s
Show

# Main menu: space-filling tiles with live teasers.
Sleep 2s

# Accounts — net worth + cash across tiers.
Type "m"
Sleep 1.8s
Type "jjj"
Sleep 1s

# Transactions — the raw stream.
Type "t"
Sleep 1.8s
Type "jjjj"
Sleep 1s

# Bills — recurring subs/bills detected from the stream.
Type "d"
Sleep 2.2s

# Invest — three-tier permanent portfolio with drift + a band breach.
Type "p"
Sleep 2.6s

# Charts — braille net-worth trend.
Type "v"
Sleep 2.2s

# Alerts — the pending queue.
Type "a"
Sleep 2.2s

# Home, then quit.
Type "o"
Sleep 1s
Type "q"
Sleep 0.6s
