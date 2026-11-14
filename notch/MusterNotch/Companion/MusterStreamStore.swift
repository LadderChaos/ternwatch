import Combine
import Foundation

struct MusterSnapshot: Decodable {
    let backend: MusterBackend?
    let agents: [MusterAgent]
    let messages: [MusterMessage]
    let error: String?
}

/// The window muster reads. herdr reports agents and their state, but no message text.
enum MusterBackend: String, Decodable {
    case tern
    case herdr

    var title: String { rawValue.uppercased() }
    var name: String { self == .tern ? "Tern" : "herdr" }
    /// What the window calls one of its terminals.
    var paneWord: String { self == .tern ? "pane" : "terminal" }
    var other: MusterBackend { self == .tern ? .herdr : .tern }
}

struct MusterAgent: Decodable, Identifiable {
    let pane: UInt64
    var id: UInt64 { pane }
    /// What the backend's commands take: the Tern pane number, or an hc address (`t7`, `vps/t7`).
    /// Absent from older watchers.
    let address: String?
    let label: String
    let title: String
    let description: String
    let cwd: String
    let session: String
    let tab: UInt32
    let open: Bool
    let omp: Bool
    let cli: String?
    let working: Bool
    /// `working`, `idle`, or herdr's own words `blocked`, `done` and `error`.
    let status: String?
    /// The herdr agent that started this one.
    let parent: UInt64?
    let quietMs: UInt64
    let model: String?
    let tokens: UInt64
    let tool: MusterTool?
    let todo: MusterTodo?
    let subagents: [MusterSubagent]

    var target: String { address ?? String(pane) }
}

struct MusterTool: Decodable {
    let name: String
    let intent: String
}

struct MusterTodo: Decodable {
    let done: UInt32
    let total: UInt32
}

struct MusterSubagent: Decodable {
    let name: String
    let running: Bool
    let done: Bool
}

struct MusterNode: Decodable {
    let pane: UInt64
    let subagent: String?
}

struct MusterMessage: Decodable {
    let kind: String
    let from: MusterNode
    let to: MusterNode
    let text: String
    let sentMs: Int64?
    let deliveredMs: Int64?
    let failed: Bool
    let outcome: String?
    let deliveryObservable: Bool
    let reply: Bool
    let replied: Bool
    let id: UInt64?
    let repeatOf: UInt64?

    init(kind: String, from: MusterNode, to: MusterNode, text: String, sentMs: Int64?, deliveredMs: Int64?,
         failed: Bool, outcome: String?, deliveryObservable: Bool, reply: Bool, replied: Bool,
         id: UInt64? = nil, repeatOf: UInt64? = nil) {
        self.kind = kind
        self.from = from
        self.to = to
        self.text = text
        self.sentMs = sentMs
        self.deliveredMs = deliveredMs
        self.failed = failed
        self.outcome = outcome
        self.deliveryObservable = deliveryObservable
        self.reply = reply
        self.replied = replied
        self.id = id
        self.repeatOf = repeatOf
    }
}

final class MusterStreamStore: ObservableObject {
    @Published private(set) var agents: [MusterAgent] = []
    @Published private(set) var messages: [MusterMessage] = []
    /// The backend the running watcher reads; nil until its first snapshot.
    @Published private(set) var backend: MusterBackend?
    /// The backend the person chose; nil lets the watcher pick (Tern when it runs, else herdr).
    @Published private(set) var choice = UserDefaults.standard.string(forKey: MusterPreferences.backend).flatMap(MusterBackend.init(rawValue:))
    @Published private(set) var error: String?
    @Published private(set) var actionError: String?
    @Published private(set) var isRunning = false

    private var process: Process?
    /// The watcher's stdin. The watcher exits when it closes, so it also stops if this app dies.
    private var input: Pipe?
    private var generation = UUID()
    private var shouldRun = false
    private var stderrBuffer = Data()
    private var retry: DispatchWorkItem?

    deinit {
        retry?.cancel()
        Self.terminate(process, input: input)
    }

    /// Closes the watcher's stdin and ends its process group. `Process` starts the watcher as a
    /// group leader, and its `tern events` or `hc events` child joins that group; terminating
    /// only the watcher would leave the child running.
    private static func terminate(_ process: Process?, input: Pipe?) {
        try? input?.fileHandleForWriting.close()
        guard let process, process.isRunning else { return }
        let pid = process.processIdentifier
        if pid > 0, getpgid(pid) == pid { kill(-pid, SIGTERM) } else { process.terminate() }
    }

    func start() {
        guard !shouldRun else { return }
        shouldRun = true
        launch()
    }

    func stop() {
        shouldRun = false
        retry?.cancel()
        retry = nil
        generation = UUID()
        Self.terminate(process, input: input)
        process = nil
        input = nil
        stderrBuffer.removeAll()
        agents = []
        messages = []
        backend = nil
        isRunning = false
        error = nil
    }

    func restart() {
        stop()
        start()
    }

    /// Watches `backend` from now on, also after relaunch.
    func choose(_ backend: MusterBackend) {
        UserDefaults.standard.set(backend.rawValue, forKey: MusterPreferences.backend)
        choice = backend
        actionError = nil
        restart()
    }

    func focus(_ agent: MusterAgent) {
        runAction(["--focus", agent.target], verb: "Focus")
    }

    func send(_ agent: MusterAgent, message: String) {
        runAction(["--send", agent.target, message], verb: "Send")
    }

    private static func environment() -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let additions = ["\(home)/.local/bin", "\(home)/.cargo/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
        environment["PATH"] = ([environment["PATH"] ?? ""] + additions).joined(separator: ":")
        return environment
    }

    private static func executable(_ name: String, override: String?, environment: [String: String], extra: [String] = []) -> String? {
        let fm = FileManager.default
        if let override = override, !override.isEmpty {
            let expanded = (override as NSString).expandingTildeInPath
            if expanded.contains("/") { return fm.isExecutableFile(atPath: expanded) ? expanded : nil }
            return (environment["PATH"] ?? "").split(separator: ":").map { "\($0)/\(expanded)" }.first { fm.isExecutableFile(atPath: $0) }
        }
        let paths = extra + (environment["PATH"] ?? "").split(separator: ":").map { "\($0)/\(name)" }
        return paths.first { fm.isExecutableFile(atPath: $0) }
    }

    /// The bundled muster built from this repository; MUSTER_BIN overrides it for development.
    private static func watcher(_ environment: [String: String]) -> String? {
        let override = environment["MUSTER_BIN"].flatMap { $0.isEmpty ? nil : $0 }
        let found = override != nil
            ? executable("muster", override: override, environment: environment)
            : Bundle.main.url(forAuxiliaryExecutable: "muster")?.path
        return found.flatMap { FileManager.default.isExecutableFile(atPath: $0) ? $0 : nil }
    }

    /// The environment muster runs in: a GUI app's PATH, plus where Tern's CLI lives.
    private static func watcherEnvironment() -> [String: String] {
        var environment = environment()
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        if let tern = executable("tern", override: environment["TERN_BIN"], environment: environment, extra: ["\(home)/.local/bin/tern", "/Applications/Tern.app/Contents/MacOS/tern"]) {
            environment["TERN_BIN"] = tern
        }
        return environment
    }

    /// `--backend` for the watcher: what it reports reading, else the person's choice, else auto.
    private var backendArguments: [String] {
        ["--backend", (backend ?? choice)?.rawValue ?? "auto"]
    }

    private func launch() {
        guard shouldRun else { return }
        let environment = Self.watcherEnvironment()
        guard let binary = Self.watcher(environment) else {
            error = "The bundled muster is missing. Rebuild Muster Notch, or set MUSTER_BIN to a muster executable."
            scheduleRetry()
            return
        }
        let child = Process()
        let stdin = Pipe()
        let stdout = Pipe()
        let stderr = Pipe()
        child.executableURL = URL(fileURLWithPath: binary)
        child.arguments = ["--json", "--follow", "--backend", choice?.rawValue ?? "auto"]
        child.environment = environment
        child.standardInput = stdin
        child.standardOutput = stdout
        child.standardError = stderr
        let current = UUID()
        generation = current
        stderrBuffer.removeAll()
        do {
            try child.run()
        } catch {
            self.error = "Could not start muster: \(error.localizedDescription)"
            scheduleRetry()
            return
        }
        process = child
        input = stdin
        isRunning = true
        error = nil
        stderr.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { handle.readabilityHandler = nil; return }
            DispatchQueue.main.async {
                guard let self = self, self.generation == current else { return }
                self.appendStderr(data)
            }
        }
        DispatchQueue.global(qos: .utility).async { [weak self] in
            var buffer = Data()
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            while true {
                let data = stdout.fileHandleForReading.availableData
                if data.isEmpty { break }
                buffer.append(data)
                var latest: Result<MusterSnapshot, Error>?
                while let newline = buffer.firstIndex(of: 0x0A) {
                    let line = Data(buffer[..<newline])
                    buffer.removeSubrange(...newline)
                    guard !line.isEmpty else { continue }
                    latest = Result { try decoder.decode(MusterSnapshot.self, from: line) }
                }
                if let latest = latest {
                    // Backpressure keeps a busy stream from queuing unlimited UI updates.
                    DispatchQueue.main.sync { [weak self] in
                        guard let self = self, self.generation == current else { return }
                        switch latest {
                        case .success(let snapshot):
                            self.agents = snapshot.agents
                            self.messages = snapshot.messages
                            self.backend = snapshot.backend ?? .tern
                            self.error = snapshot.error.map { "muster: \($0)" }
                        case .failure(let error):
                            self.error = "Invalid muster snapshot: \(error.localizedDescription)"
                        }
                    }
                }
            }
            child.waitUntilExit()
            stderr.fileHandleForReading.readabilityHandler = nil
            let remaining = stderr.fileHandleForReading.readDataToEndOfFile()
            DispatchQueue.main.async { [weak self] in
                guard let self = self, self.generation == current else { return }
                self.appendStderr(remaining)
                let detail = String(decoding: self.stderrBuffer, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                self.process = nil
                self.isRunning = false
                self.agents = []
                self.messages = []
                if detail.contains("--follow") || detail.contains("--backend") {
                    self.error = "This muster is too old for Muster Notch. Point MUSTER_BIN at a current build, or unset it to use the bundled one. \(detail)"
                } else {
                    self.error = detail.isEmpty ? "muster stopped (exit \(child.terminationStatus)). Retrying…" : "muster stopped: \(detail)"
                }
                self.scheduleRetry()
            }
        }
    }

    private func appendStderr(_ data: Data) {
        let limit = 16 * 1024
        stderrBuffer.append(data)
        if stderrBuffer.count > limit {
            stderrBuffer.removeFirst(stderrBuffer.count - limit)
        }
    }


    private func scheduleRetry() {
        guard shouldRun else { return }
        retry?.cancel()
        let task = DispatchWorkItem { [weak self] in self?.launch() }
        retry = task
        DispatchQueue.main.asyncAfter(deadline: .now() + 5, execute: task)
    }

    /// Runs `muster <arguments>` against the watched backend; it knows each window's commands.
    private func runAction(_ arguments: [String], verb: String) {
        let environment = Self.watcherEnvironment()
        guard let watcher = Self.watcher(environment) else {
            actionError = "The bundled muster is missing. Rebuild Muster Notch, or set MUSTER_BIN to a muster executable."
            return
        }
        actionError = nil
        let arguments = backendArguments + arguments
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let child = Process()
            let stderr = Pipe()
            child.executableURL = URL(fileURLWithPath: watcher)
            child.arguments = arguments
            child.environment = environment
            child.standardOutput = FileHandle.nullDevice
            child.standardError = stderr
            do {
                try child.run()
                let data = stderr.fileHandleForReading.readDataToEndOfFile()
                child.waitUntilExit()
                guard child.terminationStatus != 0 else { return }
                let detail = String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                    .replacingOccurrences(of: "Error: ", with: "")
                DispatchQueue.main.async { [weak self] in
                    self?.actionError = detail.isEmpty ? "\(verb) failed (exit \(child.terminationStatus))." : "\(verb) failed: \(detail)"
                }
            } catch {
                DispatchQueue.main.async { [weak self] in self?.actionError = "Could not run muster: \(error.localizedDescription)" }
            }
        }
    }
}
