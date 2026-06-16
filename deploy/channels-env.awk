# channels-env.awk — extract per-group channel routing from a tenant yaml into KEY=VALUE env lines
# for render-env.sh. Emits (only when present):
#   WA_CHANNELS=<l1,l2>            comma list of channel labels
#   WA_CHANNEL_<L>_URL=<base-url>  one per channel (label uppercased)
#   WA_GROUP_CHANNELS=<jid:label,...>
# A commented-out or absent `channels:` / `group_channels:` block emits nothing → default-only
# behaviour (today's wagw-1). Per-channel TOKENs are NOT emitted here: distinct per-channel bearers
# are wired in the separate multi-target operational step (render-env.sh still supports
# WA_CHANNEL_<L>_TOKEN for that); the default inbound bearer covers the common case.
#
# Expected yaml shape (two-space indent; `-` only on the first key of each list item):
#   channels:
#     - label: support
#       url: http://127.0.0.1:3002
#   group_channels:
#     - jid: "120363000000000000@g.us"
#       channel: support

function val(s) {
  sub(/^[^:]*:[[:space:]]*/, "", s)   # drop `key:` and following space
  gsub(/["\047]/, "", s)              # strip quotes (\047 = single quote)
  sub(/[[:space:]]+$/, "", s)         # trim trailing space
  return s
}

# Flush the channel currently being accumulated (label+url) into the output state.
function flush() {
  if (clabel != "") {
    labels = labels (labels == "" ? "" : ",") clabel
    print "WA_CHANNEL_" toupper(clabel) "_URL=" curl
  }
  clabel = ""; curl = ""
}

# Any top-level key (column 0, not a comment) opens/closes a block.
/^[^[:space:]#]/ {
  if ($0 ~ /^channels:/)       { flush(); inch = 1; ingc = 0; next }
  if ($0 ~ /^group_channels:/) { flush(); inch = 0; ingc = 1; next }
  flush(); inch = 0; ingc = 0; next
}

inch && /^[[:space:]]*-[[:space:]]*label:/ { flush(); clabel = val($0) }
inch && /^[[:space:]]*url:/                { curl = val($0) }
ingc && /^[[:space:]]*-[[:space:]]*jid:/   { gn++; gj[gn] = val($0); gc[gn] = "" }
ingc && /^[[:space:]]*channel:/            { gc[gn] = val($0) }

END {
  flush()
  if (labels != "") print "WA_CHANNELS=" labels
  out = ""
  for (i = 1; i <= gn; i++)
    if (gj[i] != "" && gc[i] != "")
      out = out (out == "" ? "" : ",") gj[i] ":" gc[i]
  if (out != "") print "WA_GROUP_CHANNELS=" out
}
