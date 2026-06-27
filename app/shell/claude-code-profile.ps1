# Bram manages the agent in this terminal. It autostarts the configured
# provider on launch (set shell.agent / shell.args in .bram.json) and the
# header switcher / Sessions tab change providers deliberately. Bram tracks
# the current provider host-side, so launching claude or codex by hand here
# does NOT update Bram's UI -- use the header switcher to change providers.
#
# No shell functions wrap claude/codex any more: the host owns current-agent
# state and types launch commands itself.
Write-Host "Bram manages the agent here - use the header switcher to change providers."
