Native sidebar fixture screenshots with synthetic sessions and account data.

Organize, Sort, and Show open nested menus using the same popup component
as the model selector. Organize and Sort summarize their current choices; the
ungrouped choice is named None. Show has its own section without a count, and
Compact has a separate section with a switch. Radio choices close the child;
Show and Compact toggles stay open for repeated changes.

Validation: native fixture build, changed-file formatting, and diff checks passed.
Native X11 interaction checks covered hover traversal, repeated Show toggles,
arrow-key navigation and sort selection, Compact click/Space toggling, Escape,
and outside-click dismissal.
A narrow-window check confirmed the child switches to the left when needed.
