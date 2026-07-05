---
description: First-run setup of the gw-bridge Opus-escalation rule (asks global or this project)
---

Set up gw-bridge's Opus-escalation rule for the current context.

Do these in order, do NOT skip:
1. Run `gw-bridge init --check`.
   - If it exits 0 → it's already configured here. Tell the user and STOP.
2. If it exits non-zero (2 = not configured), ASK the user EXACTLY this one question, then STOP and wait:
   "Install the Opus-escalation rule GLOBALLY (all your projects) or only in THIS project?"
3. After they answer, run the matching command:
   - global  → `gw-bridge init --scope global`
   - project → `gw-bridge init --scope project`
4. Report which files changed. If GLOBAL, remind them that `gentle-ai sync` may clobber the global
   file and `gw-bridge doctor` re-applies it.
