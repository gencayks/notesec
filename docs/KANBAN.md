# Kanban view

Any page can be shown as a kanban board instead of an outline. Toggle it
in the page header (**Outline** / **Kanban**) or with the **Kanban view**
palette command. The choice is remembered per page (in `state.toml`, so it
follows renames and survives restarts).

## The convention

Columns come from status tags on **top-level blocks**:

| Column   | Tag      |
|----------|----------|
| Unsorted | (no tag) |
| To do    | `#todo`  |
| Doing    | `#doing` |
| Done     | `#done`  |

Matching ignores case (`#TODO` works); a block's **first** status tag
decides its column, and any other `#tag` is left alone. Blocks with none
of the three tags gather in **Unsorted**, so no block is ever hidden by
the board. A card shows the block's text with the reading view's
formatting, plus how many sub-blocks it has; the whole subtree moves with
the card.

## Dragging cards

Drag a card by its grip onto another column: NoteSec cuts the old status
tag(s) out of the block and appends the new one (`#doing`, …), dropping
it in **Unsorted** removes them. That tag edit is one undo step, saved
like any edit — the markdown file stays the source of truth, so the
board, the outline and the file can never disagree. Dropping a card back
in its own column changes nothing.

## Editing and adding

- Click a card to edit it: the page returns to the outline with that
  block open for editing.
- **+ New card** under a column appends a top-level block with that
  column's tag (no tag for Unsorted) and opens it for editing in the
  outline.
