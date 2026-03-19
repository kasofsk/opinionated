# Worker Notes

Design notes for how workers should operate. To be fleshed out as the framework matures.

## Dev Log

Workers must write a summary of what they did after completing work. This serves as the dev log. The commit message is the natural place for this — write detailed, context-rich commit messages that explain what was done and why.

The dev log exists so that later workers can hydrate context quickly by reading recent summaries instead of exploring the codebase from scratch. When a worker is deciding what task to pick up next, they should:

1. Review the current on-deck jobs.
2. Read the recent dev log (commit history) to understand what's been done, what's in flight, and what's likely to be unblocked.
3. Choose a task informed by both.
