# Supervillain TODO

## Implemented

- Email listing with lazy-loading and scrolling cache
- View email (auto marks read)
- Archive, trash, mark read/unread, toggle star, move to mailbox
- Unsubscribe + archive all from sender
- Compose, reply, reply all, forward
- Auto-select matching identity for replies
- Multiple identity support (From dropdown)
- CC/BCC
- Calendar invite detection and ICS parsing
- RSVP (accept/tentative/decline)
- Add events to Fastmail calendar via CalDAV
- Gmail-style search operators (from:, to:, subject:, has:attachment, is:unread, before:, after:, newer_than:, older_than:)
- Split inbox tabs with glob/regex filters (from, to, subject, calendar)
- Auto-seed splits from Fastmail identities on first run
- Vim keybindings (j/k, gg, G, o, q, Tab/Shift+Tab)
- Command palette (Ctrl+K)
- Search bar (/)
- Help overlay (?)
- Undo toast with z to undo
- Optimistic UI updates
- Mailbox sidebar (Inbox, Archive, Trash, Sent, etc.)
- Auto-open browser on startup (webapp on Omarchy, xdg-open elsewhere)
- Config file at ~/.config/supervillain/config
- Comprehensive test suite

## Not yet implemented

- Threading/conversation grouping
- Drafts
- HTML email rendering (text-only display currently)
- Attachment download/upload
- Contact suggestions/address book
- Email signatures
- Sorting options (date descending only)
- Offline mode
