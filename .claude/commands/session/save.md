# Save Session Context

Save the current session's context for future recall.

## Instructions

1. Summarize the current session: decisions made, files changed, open questions, difficulty-book state.

2. **Session state is a block, not an archival fact** (zetetic-team-subagents memory/contract.md §8b). Write the summary to the scoped working-state block via `memory-tool.sh rethink /memories/<scope>/checkpoint.md` (use `create` for the first checkpoint); the sync drainer replicates it to Cortex tagged `memory-replica`. Do NOT call `cortex:remember` for the session summary itself.

3. If the session produced self-contained WHY-level facts (decision + rationale, rejected approach + root cause, lesson), store each via `cortex:remember` with `tags: ["archival", "<project-name>", ...]` AND `agent_topic`. Be selective.

4. Also save locally: `tools/session-store.sh save "<summary>"`

5. Confirm to the user what was saved (block path + number of archival entries, if any).

$ARGUMENTS
