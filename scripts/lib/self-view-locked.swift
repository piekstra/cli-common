// self-view-locked.swift — print "locked" when the login session's screen is
// locked, "open" otherwise. While locked, windows still list but no pixels
// can be read and no window can be raised.
import CoreGraphics

let d = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
print((d["CGSSessionScreenIsLocked"] as? Bool) == true ? "locked" : "open")
