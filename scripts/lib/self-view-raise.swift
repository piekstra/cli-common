// self-view-raise.swift <pid> — activate the app that owns <pid>, bringing
// its windows forward. self-view.sh passes only a PID from the launched tree.
// Exit 4 when no running app has that PID.
import AppKit

guard CommandLine.arguments.count == 2, let pid = Int32(CommandLine.arguments[1]),
      let app = NSRunningApplication(processIdentifier: pid) else { exit(4) }
app.activate(options: [.activateAllWindows])
usleep(400_000)
