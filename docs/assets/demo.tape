# hallouminate README demo — regenerate with docs/assets/demo-setup.sh
Output demo.gif
Set FontSize 17
Set Width 1200
Set Height 730
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type 'cd "$HALLOUMINATE_DEMO/hallouminate" && clear'
Enter
Show

Type "# your agent re-learns your codebase every session."
Enter
Type "# hallouminate gives it a per-repo wiki it writes — and searches."
Enter
Sleep 1.5s

Type 'hallouminate ground "why is the filesystem the source of truth?" --top-files 2 --chunks-per-file 2 --snippet-chars 150'
Enter
Sleep 4.5s

Type "ls .hallouminate/wiki/ | head -6"
Enter
Sleep 2.5s

Type "# plain markdown on disk — read it, edit it, commit it with your code."
Enter
Sleep 4s