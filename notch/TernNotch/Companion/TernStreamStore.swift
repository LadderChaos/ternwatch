import Combine
import Foundation

struct TernSnapshot: Decodable {
    let agents: [TernAgent]
    let messages: [TernMessage]
    let error: String?
}

struct TernAgent: Decodable, Identifiable {
    let pane: UInt64
    var id: UInt64 { pane }
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
    let quietMs: UInt64
    let model: String?
    let tokens: UInt64
    let tool: TernTool?
    let todo: TernTodo?
    let subagents: [TernSubagent]
}

struct TernTool: Decodable {
    let name: String
    let intent: String
}

struct TernTodo: Decodable {
    let done: UInt32
    let total: UInt32
}

struct TernSubagent: Decodable {
    let name: String
    let running: Bool
    let done: Bool
}

struct TernNode: Decodable {
    let pane: UInt64
    let subagent: String?
}

struct TernMessage: Decodable {
    let kind: String
    let from: TernNode
    let to: TernNode
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

    init(kind: String, from: TernNode, to: TernNode, text: String, sentMs: Int64?, deliveredMs: Int64?,
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

final class TernStreamStore: ObservableObject {
    @Published private(set) var agents: [TernAgent] = []
    @Published private(set) var messages: [TernMessage] = []
    @Published private(set) var error: String?
    @Published private(set) var actionError: String?
    @Published private(set) var isRunning = false

    private var process: Process?
    private var generation = UUID()
    private var shouldRun = false
    private var stderrBuffer = Data()
    private var retry: DispatchWorkItem?

    deinit {
        retry?.cancel()
        if let process = process, process.isRunning { process.terminate() }
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
        if let process = process, process.isRunning { process.terminate() }
        process = nil
        stderrBuffer.removeAll()
        agents = []
        messages = []
        isRunning = false
        error = nil
    }

    func restart() {
        stop()
        start()
    }

    func focus(_ pane: UInt64) {
        runAction(["focus", String(pane)])
    }

    func send(_ pane: UInt64, message: String) {
        runAction(["run", String(pane), message])
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

    private func launch() {
        guard shouldRun else { return }
        let environment = Self.environment()
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        // The app bundles the ternwatch built from this repository; TERNWATCH_BIN overrides it for development.
        let override = environment["TERNWATCH_BIN"].flatMap { $0.isEmpty ? nil : $0 }
        let found = override != nil
            ? Self.executable("ternwatch", override: override, environment: environment)
            : Bundle.main.url(forAuxiliaryExecutable: "ternwatch")?.path
        guard let binary = found, FileManager.default.isExecutableFile(atPath: binary) else {
            error = "The bundled ternwatch is missing. Rebuild Tern Notch, or set TERNWATCH_BIN to a ternwatch executable."
            scheduleRetry()
            return
        }
        var childEnvironment = environment
        if let tern = Self.executable("tern", override: environment["TERN_BIN"], environment: environment, extra: ["\(home)/.local/bin/tern", "/Applications/Tern.app/Contents/MacOS/tern"]) {
            childEnvironment["TERN_BIN"] = tern
        }
        let child = Process()
        let stdout = Pipe()
        let stderr = Pipe()
        child.executableURL = URL(fileURLWithPath: binary)
        child.arguments = ["--json", "--follow"]
        child.environment = childEnvironment
        child.standardOutput = stdout
        child.standardError = stderr
        let current = UUID()
        generation = current
        stderrBuffer.removeAll()
        do {
            try child.run()
        } catch {
            self.error = "Could not start ternwatch: \(error.localizedDescription)"
            scheduleRetry()
            return
        }
        process = child
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
                var latest: Result<TernSnapshot, Error>?
                while let newline = buffer.firstIndex(of: 0x0A) {
                    let line = Data(buffer[..<newline])
                    buffer.removeSubrange(...newline)
                    guard !line.isEmpty else { continue }
                    latest = Result { try decoder.decode(TernSnapshot.self, from: line) }
                }
                if let latest = latest {
                    // Backpressure keeps a busy stream from queuing unlimited UI updates.
                    DispatchQueue.main.sync { [weak self] in
                        guard let self = self, self.generation == current else { return }
                        switch latest {
                        case .success(let snapshot):
                            self.agents = snapshot.agents
                            self.messages = snapshot.messages
                            self.error = snapshot.error.map { "ternwatch: \($0)" }
                        case .failure(let error):
                            self.error = "Invalid ternwatch snapshot: \(error.localizedDescription)"
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
                if detail.contains("--follow") {
                    self.error = "ternwatch does not support --follow. Point TERNWATCH_BIN at a current build, or unset it to use the bundled one. \(detail)"
                } else {
                    self.error = detail.isEmpty ? "ternwatch stopped (exit \(child.terminationStatus)). Retrying…" : "ternwatch stopped: \(detail)"
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

    private func runAction(_ arguments: [String]) {
        let environment = Self.environment()
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        guard let tern = Self.executable("tern", override: environment["TERN_BIN"], environment: environment, extra: ["\(home)/.local/bin/tern", "/Applications/Tern.app/Contents/MacOS/tern"]) else {
            actionError = "Cannot find tern. Install Tern or set TERN_BIN to its executable path."
            return
        }
        actionError = nil
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let child = Process()
            let stderr = Pipe()
            child.executableURL = URL(fileURLWithPath: tern)
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
                DispatchQueue.main.async { [weak self] in
                    self?.actionError = detail.isEmpty ? "tern \(arguments[0]) failed (exit \(child.terminationStatus))." : "tern \(arguments[0]) failed: \(detail)"
                }
            } catch {
                DispatchQueue.main.async { [weak self] in self?.actionError = "Could not run tern \(arguments[0]): \(error.localizedDescription)" }
            }
        }
    }
}
