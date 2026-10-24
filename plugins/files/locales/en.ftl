# File manager messages. Key names and the deletion token "yes" are literal.

action-sidebar = Files: sidebar
action-pane = Files: pane
action-picker = Files: pick a path
title-files = Files
title-picker = Pick a path
title-preview = Preview
flag-remote = remote
flag-pinned = pinned
remote-unavailable = { $host }: remote browsing is not available
conflict = { $name } exists: o overwrite, s skip, r rename; O/S/R for all; Esc stops
empty = empty
no-match = no match
sort-name = name
sort-mtime = mtime
sort-size = size
sort-ext = ext
layout-list = list
layout-tree = tree
layout-columns = columns
view-changed = view: { $layout }
marked = { $count } marked
visual = VISUAL
clipboard-copy = { $count } to copy (P)
clipboard-move = { $count } to move (P)
prefix-mark = mark-
prefix-jump = jump-
prefix-sort = sort: n name, m time, s size, e ext, d dirs, r reverse
picker-insert = ⏎ insert
no-insert-pane = no pane to insert into
no-pane = no pane
not-at-prompt = the pane is not at a prompt
copied-paths = copied { $paths }
changed-directory = cd { $path }
mark-set = '{ $key } → { $path }
path-gone = { $path }: gone
no-mark = no mark '{ $key }
zoxide-prompt = zoxide:
zoxide-no-match = zoxide: no match for { $query }
zoxide-error = zoxide: { $error }
following = following the focused pane
pinned = pinned
preview-size = { $size } ({ $count } bytes)
preview-modified = modified { $date }
preview-too-big = too big to preview
preview-pixels = { $width } × { $height } px
preview-helper-error = no preview ({ $program }: { $error })
preview-listing-error = no listing (bsdtar: { $error })
failed = failed
preview-failed = no preview: { $error }
plugin-error = Files: { $error }
svg-size-error = SVG: invalid image size
nothing-to-paste = nothing to paste (C or x first)
operation-busy = another copy or move is running
task-undo = Files: undoing a move
task-copy = Files: copying
task-move = Files: moving
progress = { $done } of { $total }
undo-unavailable = undo unavailable for merged, replaced, or partial moves
rename-prompt = Rename to:
new-prompt = New file (end with / for a directory):
name-slash = a name cannot hold /
name-exists = { $name } exists
stay-inside = stay inside this directory
bad-mode = bad mode `{ $mode }`
not-deleted = not deleted
nothing-to-link = nothing to link (C first)
nothing-to-undo = nothing to undo
bulk-one-directory = bulk rename: files of one directory
bulk-newline = bulk rename: a name holds a newline
bulk-no-change = no names changed
copy-self = cannot go into itself
copy-type-conflict = cannot replace a directory with a file or a file with a directory
copy-unsupported = this file type cannot be copied
rename-count-mismatch = { $names } names for { $count } files; nothing renamed
rename-bad-name = bad name `{ $name }`; nothing renamed
rename-duplicate = `{ $name }` twice; nothing renamed
rename-exists = { $path } exists; nothing renamed
not-in-trash = { $path }: not in the trash
restore-exists = { $path } exists again
no-parent = no parent directory
no-file-name = no file name
inserted-paths = { $count ->
    [one] inserted { $count } path
   *[other] inserted { $count } paths
}
archive-entries = { $count ->
    [one] { $count } entry
   *[other] { $count } entries
}
yank-copy = { $count ->
    [one] { $count } file to copy: P pastes
   *[other] { $count } files to copy: P pastes
}
yank-move = { $count ->
    [one] { $count } file to move: P pastes
   *[other] { $count } files to move: P pastes
}
copied-files = { $count ->
    [one] { $count } file copied
   *[other] { $count } files copied
}
moved-files = { $count ->
    [one] { $count } file moved
   *[other] { $count } files moved
}
copy-errors = { $count ->
    [one] { $count } copied; { $failed } failed: { $error }
   *[other] { $count } copied; { $failed } failed: { $error }
}
move-errors = { $count ->
    [one] { $count } moved; { $failed } failed: { $error }
   *[other] { $count } moved; { $failed } failed: { $error }
}
chmod-prompt = { $count ->
    [one] chmod { $count } file (e.g. 644, u+x):
   *[other] chmod { $count } files (e.g. 644, u+x):
}
delete-prompt = { $count ->
    [one] Delete { $count } file for good? Type yes:
   *[other] Delete { $count } files for good? Type yes:
}
delete-errors = { $count ->
    [one] { $count } not deleted: { $error }
   *[other] { $count } not deleted: { $error }
}
deleted-files = { $count ->
    [one] deleted { $count } file
   *[other] deleted { $count } files
}
renamed-files = { $count ->
    [one] renamed { $count } file
   *[other] renamed { $count } files
}
trashed-files = { $count ->
    [one] trashed { $count } file (u: undo)
   *[other] trashed { $count } files (u: undo)
}
undid-renames = { $count ->
    [one] undid { $count } rename
   *[other] undid { $count } renames
}
restored-files = { $count ->
    [one] restored { $count } file
   *[other] restored { $count } files
}
bulk-confirm = { $count ->
    [one] Rename { $count } file?
   *[other] Rename { $count } files?
}

a11y-files = Files
a11y-filter = Filter files
a11y-directory = Directory
a11y-file = File
a11y-preview = Preview
