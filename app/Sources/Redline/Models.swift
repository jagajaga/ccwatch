import Foundation

// Mirrors the `Snapshot` JSON that ccwatchd streams over the Unix socket.
// Decoded with `.convertFromSnakeCase`, so Swift properties are camelCase.
// Unknown keys (e.g. "msg", usage_buckets) are ignored by Codable.

struct Snapshot: Decodable {
    var generatedAt: Int64 = 0
    var sessions: [Session] = []
    var alerts: [Alert] = []
    var totals: Totals = Totals()
    var modelMix: [MixEntry] = []
    var governor: GovernorStatus?
    var pacing: PacingPlan?
}

struct Totals: Decodable {
    var activeSessions: Int = 0
    var tokensPerMin: Double = 0
    var totalTokens: UInt64 = 0
    var cacheHitPct: Double = 0
}

struct GovernorStatus: Decodable {
    var window: Tank
    var week: Tank?
}

struct PaceAction: Decodable {
    var op: String = ""
    var pid: Int?
    var reason: String?
}

struct PacingPlan: Decodable {
    var targetRate: Double = 0
    var actualRate: Double = 0
    var price: Double = 0
    var actions: [PaceAction] = []
    var reason: String = ""
    /// True when the daemon is autonomously enforcing this plan (mode "auto").
    var auto: Bool = false
    /// How many sessions Cruise currently has paused (autonomous mode).
    var paced: Int = 0
}

struct Tank: Decodable {
    var used: UInt64 = 0
    var budget: UInt64?
    var budgetSource: String = "unknown"
    var windowStart: Int64 = 0
    var resetsAt: Int64?
    var ratePerMin: Double = 0
    var cruisePerMin: Double?
    var delta: Double?
    var rangeMin: Double?
    var wallAt: Int64?

    /// Fraction of the tank consumed (0…1), or nil when no budget is known.
    var usedFrac: Double? {
        guard let b = budget, b > 0 else { return nil }
        return min(1.0, Double(used) / Double(b))
    }
}

struct Session: Decodable, Identifiable {
    var id: String
    var name: String = ""
    var title: String?
    var cwd: String = ""
    var pid: Int?
    var kind: String = ""
    var entrypoint: String = ""
    var version: String = ""
    var model: String?
    var state: String = "idle"
    var startedAt: Int64?
    var lastActivity: Int64?
    var tokens: Ledger = Ledger()
    var tokensPerMin: Double = 0
    var cpuPct: Double = 0
    var rssMb: UInt64 = 0
    var agents: [Agent] = []
    var tasks: [Task] = []
    var watchers: [Watcher] = []
    var activity: [Activity] = []
    var processes: [ProcInfo] = []
    var host: Host = Host()
    var remoteName: String?
}

struct Ledger: Decodable {
    var input: UInt64 = 0
    var output: UInt64 = 0
    var cacheWrite: UInt64 = 0
    var cacheRead: UInt64 = 0
    var webSearch: UInt64 = 0
    var webFetch: UInt64 = 0
    var messages: UInt64 = 0

    var total: UInt64 { input + output + cacheWrite + cacheRead }
}

struct Agent: Decodable, Identifiable {
    var id: String
    var subagentType: String = ""
    var description: String = ""
    var model: String?
    var state: String = "running"
    var startedAt: Int64?
    var tokens: Ledger = Ledger()
    var tokensPerMin: Double = 0
    var activity: [Activity] = []
    var lastActivity: Int64?
    var children: [Agent] = []
}

struct Task: Decodable {
    var subject: String = ""
    var status: String = ""
    var blocked: Bool = false
    var activeForm: String?
}

struct Activity: Decodable {
    var tool: String = ""
    var detail: String = ""
    var sinceMs: Int64 = 0
}

struct ProcInfo: Decodable, Identifiable {
    var pid: Int = 0
    var name: String = ""
    var cmd: String = ""
    var cpuPct: Double = 0
    var rssMb: UInt64 = 0
    var runSecs: UInt64 = 0
    var id: Int { pid }
}

struct Watcher: Decodable {
    var kind: String = ""
    var name: String = ""
    var detail: String = ""
    var schedule: String?
    var lastFired: Int64?
    var firedCount: UInt64 = 0
    var nextWake: Int64?
    var running: Bool = false
    var pid: Int?
}

struct Alert: Decodable, Identifiable {
    var severity: String = "warn"
    var kind: String = ""
    var subject: String = ""
    var sessionId: String = ""
    var message: String = ""
    var sinceMs: Int64 = 0
    var id: String { "\(kind)|\(sessionId)|\(subject)" }
}

struct Host: Decodable {
    var kind: String = "local"
    var name: String?
    var sshTarget: String?
}

/// `model_mix` is serialized as an array of `[tier, tokens]` pairs.
struct MixEntry: Decodable, Identifiable {
    var tier: String
    var tokens: UInt64
    var id: String { tier }

    init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        tier = try c.decode(String.self)
        tokens = try c.decode(UInt64.self)
    }
}
