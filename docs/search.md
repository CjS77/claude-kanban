# Search syntax

Terms are separated by commas and *all* must match. A bare term is a phrase looked for anywhere in a ticket — id,
title, body, labels, branch, external binding, PR.

| Key | Meaning |
|-----|---------|
| `text:` | that same phrase, spelled out |
| `label:` | one of the ticket's labels |
| `epic:` | its epic, by id or title — `none` / `null` for tickets under no epic |
| `id:` | the ticket id |
| `note:` | progress notes — kept out of bare text |
| `status:` | draft / stub / review / ready |
| `col:` `column:` | todo / doing / review / done |
| `model:` | the model the ticket asks to be worked with |
| `effort:` | low / medium / high / xhigh / max |
| `landed:` | done and not discarded |
| `discarded:` | abandoned rather than shipped |
| `blocked:` | waiting on a dependency |
| `auto-merge:` | merges itself once review passes — the ticket's own flag or the epic's it inherits |

Values match as substrings; `status:` and `effort:` take any prefix, so `status:re` is review and `effort:x` is
xhigh. Booleans read `true`/`yes`/`y`/`1`/`on` and their negatives.

Quote a value to keep a comma inside it (`label:"foo, bar"`); quoting a whole term forces it to plain text. An
unknown key or a value that doesn't fit is searched as plain text — never an error.

## Example

```
landed:true, label:ux, realtime results
```

Three ANDed terms: only cards that are landed, wear the `ux` label, AND contain "realtime results" somewhere.
