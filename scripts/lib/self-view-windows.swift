// self-view-windows.swift — list on-screen windows for scripts/self-view.sh.
//
// Prints one line per on-screen window, front to back:
//   <owner-pid> <window-id> <layer> <x> <y> <width> <height>
// (points, top-left origin). It reports facts only; choosing a window is
// self-view.sh's job, which keeps only windows owned by the process tree it
// launched. CGWindowList is used because AX/AppleScript often cannot see a
// webview window (Tauri, Electron) at all. Exit 5, with a message on stderr,
// when the window server returns no list, so the caller reports that rather
// than "no window".
import CoreGraphics
import Foundation

let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
guard let ws = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
  FileHandle.standardError.write("CGWindowListCopyWindowInfo returned nil\n".data(using: .utf8)!)
  exit(5)
}
for w in ws {
  guard let pid = w[kCGWindowOwnerPID as String] as? Int32,
        let id = w[kCGWindowNumber as String] as? Int else { continue }
  let layer = w[kCGWindowLayer as String] as? Int ?? 0
  let b = w[kCGWindowBounds as String] as? [String: CGFloat] ?? [:]
  print(pid, id, layer, Int(b["X"] ?? 0), Int(b["Y"] ?? 0), Int(b["Width"] ?? 0), Int(b["Height"] ?? 0))
}
