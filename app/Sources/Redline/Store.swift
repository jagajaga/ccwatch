import Foundation
import Darwin

/// Talks to ccwatchd over the Unix socket: spawns it if needed, subscribes,
/// and streams decoded `Snapshot`s. Networking runs on a background thread;
/// callbacks are delivered on the main queue.
final class DaemonClient: @unchecked Sendable {
    var onSnapshot: ((Snapshot) -> Void)?
    var onConnection: ((Bool) -> Void)?

    // Coalesced main-thread delivery: the daemon pushes ~2/sec, but if the UI is
    // mid-render the async closures (each holding a whole Snapshot) would pile up
    // on the main queue unboundedly — a runaway memory leak. Keep only the latest
    // pending snapshot and schedule at most one delivery at a time.
    private let deliverLock = NSLock()
    private var pendingSnap: Snapshot?
    private var deliveryScheduled = false

    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    private var socketPath: String {
        let base = ProcessInfo.processInfo.environment["CLAUDE_CONFIG_DIR"]
            ?? (NSHomeDirectory() + "/.claude")
        return base + "/ccwatch/daemon.sock"
    }

    private func daemonBinary() -> String {
        if let exe = Bundle.main.executablePath {
            let dir = (exe as NSString).deletingLastPathComponent
            let cand = dir + "/ccwatchd"
            if FileManager.default.isExecutableFile(atPath: cand) { return cand }
        }
        return "ccwatchd"
    }

    func start() {
        Thread.detachNewThread { [weak self] in
            self?.runLoop()
        }
    }

    private func runLoop() {
        while true {
            ensureDaemon()
            let fd = connectSocket()
            if fd < 0 {
                notifyConnection(false)
                sleepMs(1000)
                continue
            }
            subscribe(fd)
            notifyConnection(true)
            readMessages(fd)
            close(fd)
            notifyConnection(false)
            sleepMs(800) // dropped; retry
        }
    }

    private func ensureDaemon() {
        let fd = connectSocket()
        if fd >= 0 { close(fd); return }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: daemonBinary())
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        try? p.run()
        for _ in 0..<40 {
            let f = connectSocket()
            if f >= 0 { close(f); return }
            sleepMs(100)
        }
    }

    private func subscribe(_ fd: Int32) {
        let line = "{\"msg\":\"subscribe\"}\n"
        _ = line.withCString { write(fd, $0, strlen($0)) }
    }

    /// The daemon serializes `ServerMsg` tag-first, so a snapshot line always
    /// begins with this. A cheap prefix test lets us skip heartbeats/results
    /// without parsing them, and — crucially — avoids decoding a heartbeat into
    /// an all-default (empty) Snapshot.
    private let snapshotPrefix = Array(#"{"msg":"snapshot""#.utf8)

    private func readMessages(_ fd: Int32) {
        let cap = 1 << 16
        var buf = [UInt8](repeating: 0, count: cap)
        var acc = Data()
        while true {
            // Drain an autorelease pool every read: this loop never returns, and
            // JSONDecoder autoreleases large temporaries per 240 KB snapshot — with
            // no pool they pile up into gigabytes. This is the memory-leak fix.
            let alive = autoreleasepool { () -> Bool in
                let n = read(fd, &buf, cap)
                if n <= 0 { return false }
                acc.append(contentsOf: buf[0..<n])
                while let nl = acc.firstIndex(of: 0x0A) {
                    let line = acc.subdata(in: acc.startIndex..<nl)
                    acc.removeSubrange(acc.startIndex...nl)
                    handleLine(line)
                }
                return true
            }
            if !alive { return }
        }
    }

    private func handleLine(_ line: Data) {
        // Only snapshots matter; skip heartbeats/action-results with a cheap
        // prefix check instead of a second full JSON parse (the CPU half).
        guard line.starts(with: snapshotPrefix) else { return }
        guard let snap = try? decoder.decode(Snapshot.self, from: line) else { return }
        deliverLatest(snap)
    }

    /// Deliver the newest snapshot to the main thread, coalescing so a slow UI
    /// can never make the main queue back up with stale snapshots.
    private func deliverLatest(_ snap: Snapshot) {
        deliverLock.lock()
        pendingSnap = snap
        if deliveryScheduled {
            deliverLock.unlock()
            return
        }
        deliveryScheduled = true
        deliverLock.unlock()

        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.deliverLock.lock()
            let snap = self.pendingSnap
            self.pendingSnap = nil
            self.deliveryScheduled = false
            self.deliverLock.unlock()
            if let snap { self.onSnapshot?(snap) }
        }
    }

    /// Fire an action (kill/pause/resume) on a fresh connection.
    func sendAction(_ json: [String: Any]) {
        DispatchQueue.global().async {
            let fd = self.connectSocket()
            if fd < 0 { return }
            defer { close(fd) }
            guard var data = try? JSONSerialization.data(withJSONObject: json) else { return }
            data.append(0x0A)
            data.withUnsafeBytes { raw in
                if let base = raw.baseAddress { _ = write(fd, base, data.count) }
            }
            // Drain one reply so the daemon completes the action.
            var tmp = [UInt8](repeating: 0, count: 4096)
            _ = read(fd, &tmp, tmp.count)
        }
    }

    // MARK: - POSIX plumbing

    private func connectSocket() -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 { return -1 }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = socketPath.utf8CString
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path)
        if pathBytes.count > maxLen { close(fd); return -1 }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: maxLen) { dst in
                pathBytes.withUnsafeBufferPointer { src in
                    dst.update(from: src.baseAddress!, count: pathBytes.count)
                }
            }
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        let res = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, len)
            }
        }
        if res != 0 { close(fd); return -1 }
        // Receive timeout so a stalled/half-open connection can never wedge the
        // read loop forever — the daemon heartbeats every ~1s, so a healthy
        // connection never trips this; a dead one forces a reconnect within 25s.
        var tv = timeval(tv_sec: 25, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        return fd
    }

    private func notifyConnection(_ up: Bool) {
        let cb = onConnection
        DispatchQueue.main.async { cb?(up) }
    }

    private func sleepMs(_ ms: UInt32) { usleep(ms * 1000) }
}

/// The observable app state: the latest snapshot, published to SwiftUI.
@MainActor
final class Store: ObservableObject {
    @Published var snap: Snapshot?
    @Published var connected = false
    /// Rolling burn-rate samples (tokens/min) for the menu-bar load graph.
    @Published var burnHistory: [Double] = []

    private let historyCap = 48
    private let client = DaemonClient()

    func start() {
        client.onSnapshot = { [weak self] snap in
            guard let self else { return }
            self.snap = snap
            self.connected = true
            self.burnHistory.append(snap.totals.tokensPerMin)
            if self.burnHistory.count > self.historyCap {
                self.burnHistory.removeFirst(self.burnHistory.count - self.historyCap)
            }
        }
        client.onConnection = { [weak self] up in
            if !up { self?.connected = false }
        }
        client.start()
    }

    // Fire-and-forget forwarders — no main-actor state touched, so they're
    // callable straight from SwiftUI button actions on any Swift toolchain.
    nonisolated func killSession(pid: Int) {
        client.sendAction(["msg": "action", "action": "kill_session", "pid": pid])
    }
    nonisolated func pauseSession(pid: Int) {
        client.sendAction(["msg": "action", "action": "pause_session", "pid": pid])
    }
    nonisolated func resumeSession(pid: Int) {
        client.sendAction(["msg": "action", "action": "resume_session", "pid": pid])
    }
    nonisolated func applyPacing() {
        client.sendAction(["msg": "action", "action": "apply_pacing"])
    }
    nonisolated func setCruiseMode(_ mode: String) {
        client.sendAction(["msg": "action", "action": "set_cruise_mode", "mode": mode])
    }
}
