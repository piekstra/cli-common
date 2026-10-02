// self-view-post.swift {move|click} <x> <y> — post a pointer move, or a left
// click, at a screen point (points, top-left origin).
//
// Exit 0 once the pointer reads back at the point; 3 when this process may
// not post events (Accessibility not granted, so the OS would drop them
// silently); 2 on bad arguments; 5 when an event cannot be built or the
// pointer is not where it was sent.
import CoreGraphics
import Foundation

let args = CommandLine.arguments
guard args.count == 4, args[1] == "move" || args[1] == "click",
      let x = Double(args[2]), let y = Double(args[3]) else { exit(2) }
guard CGPreflightPostEventAccess() else { exit(3) }
let pt = CGPoint(x: x, y: y)

func post(_ type: CGEventType) {
  guard let e = CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: pt, mouseButton: .left) else { exit(5) }
  e.post(tap: .cghidEventTap)
  usleep(60_000)
}
post(.mouseMoved)
if args[1] == "click" {
  post(.leftMouseDown)
  post(.leftMouseUp)
}
guard let now = CGEvent(source: nil)?.location, abs(now.x - x) <= 1, abs(now.y - y) <= 1 else { exit(5) }
