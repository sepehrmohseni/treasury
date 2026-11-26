# 0005: The row limit is inserted into the user's text and verified by parsing again

## Context

The brief asks for the default row limit to be added by rewriting the AST, never by string
concatenation. Printing a modified AST would change the statement the user typed: comments go,
quoting and spacing change, and the server's error positions no longer match the editor.

## Decision

The AST decides: only a top-level SELECT with no limit, no locking clause, no `INTO`, no
data-changing CTE and no `TOP` qualifies. The clause (`LIMIT n`, `TOP n`, `FETCH FIRST n ROWS
ONLY`) is inserted at the position the dialect's grammar requires, found from the token stream
(after the last significant token, or after `SELECT [DISTINCT]`). The result is parsed again, and
it is used only if its AST equals the original AST with exactly that limit set. Anything else
reads at most the limit and leaves the remaining rows on the server.

## Consequences

The user's text, comments and formatting stay as written, and error positions map back exactly
(everything after the insertion point is shifted by its length). Statements the parser cannot
handle are never rewritten, only cut off.
