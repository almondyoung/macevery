import Combine
import CryptoKit
import Foundation

private struct StartupCheckFailure: Error, CustomStringConvertible {
    let description: String
}

private func requireStartup(_ value: @autoclosure () -> Bool, _ message: String) throws {
    if !value() { throw StartupCheckFailure(description: message) }
}

private func startupDigest(_ value: String) -> String {
    SHA256.hash(data: Data(value.utf8)).map { String(format: "%02x", $0) }.joined()
}

private func startupPump(until condition: () -> Bool, timeout: TimeInterval) throws {
    let deadline = ProcessInfo.processInfo.systemUptime + timeout
    while !condition() && ProcessInfo.processInfo.systemUptime < deadline {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
    }
    try requireStartup(condition(), "Timed out waiting for owned integration condition")
}

private func startupPump(for duration: TimeInterval) {
    let end = ProcessInfo.processInfo.systemUptime + duration
    while ProcessInfo.processInfo.systemUptime < end {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
    }
}

private struct StartupConfiguration: Decodable {
    let root: String
    let wrapper: String
    let port: Int
    let token: String
    let query: String
    let nohit: String
    let expected_count: Int
    let timeout: TimeInterval
}

private final class StartupTransport: @unchecked Sendable {
    let configuration: StartupConfiguration
    let root: URL
    let session: URLSession
    private let lock = NSLock()
    private var sequence = 0
    private var events: [[String: Any]] = []
    private var delay: TimeInterval = 0
    private var phase = "initial"

    init(_ configuration: StartupConfiguration) {
        self.configuration = configuration
        root = URL(fileURLWithPath: configuration.root)
        let settings = URLSessionConfiguration.ephemeral
        settings.protocolClasses = [] // Built-in HTTP only; never recurse into this adapter.
        settings.timeoutIntervalForRequest = 15
        settings.timeoutIntervalForResource = configuration.timeout
        session = URLSession(configuration: settings)
    }

    func setPhase(_ phase: String, deliveryDelay: TimeInterval = 0) {
        lock.lock()
        self.phase = phase
        delay = deliveryDelay
        lock.unlock()
    }

    func begin(_ request: URLRequest) -> (Int, TimeInterval) {
        lock.lock()
        sequence += 1
        let id = sequence
        let pause = delay
        appendLocked("forward_start", id: id, extra: [
            "path": request.url?.path ?? "", "query_sha256": startupDigest(Self.query(request)),
            "request_timeout_seconds": request.timeoutInterval
        ])
        lock.unlock()
        return (id, pause)
    }

    static func query(_ request: URLRequest) -> String {
        URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems?
            .first(where: { $0.name == "q" })?.value ?? ""
    }

    func record(_ kind: String, id: Int, extra: [String: Any] = [:]) {
        lock.lock()
        appendLocked(kind, id: id, extra: extra)
        lock.unlock()
    }

    private func appendLocked(_ kind: String, id: Int, extra: [String: Any]) {
        var entry = extra
        entry.merge(["kind": kind, "id": id, "phase": phase,
                     "uptime": ProcessInfo.processInfo.systemUptime,
                     "epoch_seconds": Date().timeIntervalSince1970]) { _, new in new }
        events.append(entry)
        let data = try! JSONSerialization.data(withJSONObject: entry, options: [.sortedKeys]) + Data([10])
        let url = root.appendingPathComponent("transport.events.jsonl")
        if !FileManager.default.fileExists(atPath: url.path) { _ = FileManager.default.createFile(atPath: url.path, contents: nil) }
        let file = try! FileHandle(forWritingTo: url)
        defer { try? file.close() }
        try! file.seekToEnd()
        try! file.write(contentsOf: data)
    }

    func countSearch(_ query: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return events.filter { ($0["kind"] as? String) == "forward_start"
            && ($0["path"] as? String) == "/search" && ($0["query_sha256"] as? String) == startupDigest(query) }.count
    }

    func deliveredHTTP200(phase: String, query: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let ids = Set(events.filter { $0["kind"] as? String == "forward_start" && $0["phase"] as? String == phase
            && $0["path"] as? String == "/search" && $0["query_sha256"] as? String == startupDigest(query) }
            .compactMap { $0["id"] as? Int })
        let successes = Set(events.filter { $0["kind"] as? String == "underlying_complete" && $0["status"] as? Int == 200 && $0["url_error_code"] == nil }
            .compactMap { $0["id"] as? Int })
        return events.contains { $0["kind"] as? String == "delivered" && ($0["id"] as? Int).map { ids.contains($0) && successes.contains($0) } == true }
    }
}

// All model requests are intercepted. Only the owned random loopback endpoint
// is reachable, and its token replaces the model's unrelated generated token.
private final class StartupProtocol: URLProtocol, @unchecked Sendable {
    static var transport: StartupTransport!
    private let lock = NSLock()
    private var stopped = false
    private var underlyingTask: URLSessionDataTask?
    private var delivery: DispatchWorkItem?
    private var id = 0

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let context = Self.transport!
        guard let url = request.url, url.scheme == "http", url.host == "127.0.0.1", url.port == 17649,
              ["/search", "/status"].contains(url.path),
              var target = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
            client?.urlProtocol(self, didFailWithError: URLError(.badURL))
            return
        }
        let (id, delay) = context.begin(request)
        self.id = id
        target.port = context.configuration.port
        var forwarded = request
        forwarded.url = target.url!
        forwarded.setValue("Bearer \(context.configuration.token)", forHTTPHeaderField: "Authorization")
        let task = context.session.dataTask(with: forwarded) { [weak self] data, response, error in
            guard let self else { return }
            var receipt: [String: Any] = ["status": (response as? HTTPURLResponse)?.statusCode ?? 0]
            if let error { receipt["url_error_code"] = (error as NSError).code }
            if let data, data.count <= 8 * 1024 * 1024 {
                try? data.write(to: context.root.appendingPathComponent(String(format: "http-%04d.raw.json", id)), options: .atomic)
                receipt["bytes"] = data.count
            }
            context.record("underlying_complete", id: id, extra: receipt)
            let delivery = DispatchWorkItem { [weak self] in
                guard let self else { return }
                self.lock.lock()
                let stopped = self.stopped
                self.lock.unlock()
                guard !stopped else { return }
                context.record("delivered", id: id)
                if let error {
                    self.client?.urlProtocol(self, didFailWithError: error)
                } else if let response, (data?.count ?? 0) <= 8 * 1024 * 1024 {
                    self.client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
                    if let data { self.client?.urlProtocol(self, didLoad: data) }
                    self.client?.urlProtocolDidFinishLoading(self)
                } else {
                    self.client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse))
                }
            }
            self.lock.lock()
            self.delivery = delivery
            let stopped = self.stopped
            self.lock.unlock()
            if !stopped { DispatchQueue.global().asyncAfter(deadline: .now() + delay, execute: delivery) }
        }
        lock.lock()
        underlyingTask = task
        let stopped = self.stopped
        lock.unlock()
        if !stopped { task.resume() } else { task.cancel() }
    }

    override func stopLoading() {
        lock.lock()
        stopped = true
        let task = underlyingTask
        delivery?.cancel()
        lock.unlock()
        task?.cancel()
        Self.transport.record("cancelled_underlying", id: id)
    }
}

@main
private enum StartupIntegrationChecks {
    static func main() {
        if CommandLine.arguments.dropFirst().contains("--help") {
            print("StartupIntegrationChecks --config /tmp/owned/config.local.json")
            return
        }
        do { try run() }
        catch {
            fputs("FAIL: \(error)\n", stderr)
            exit(1)
        }
    }

    private static func run() throws {
        try requireStartup(CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--config", "Expected --config only")
        let config = try JSONDecoder().decode(StartupConfiguration.self,
            from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
        let root = URL(fileURLWithPath: config.root)
        try requireStartup(config.port > 1024 && config.port < 65536 && config.port != 17649, "Unsafe owned service port")
        func loadOracle(_ name: String) throws -> [SearchResult] {
            let data = try Data(contentsOf: root.appendingPathComponent(name))
            let raw = try JSONSerialization.jsonObject(with: data) as? [[String: Any]]
            let fields: Set<String> = ["path", "basename", "kind", "size", "mtime", "score"]
            try requireStartup(raw != nil && raw!.allSatisfy { Set($0.keys) == fields }, "Oracle must have exactly six complete result fields")
            return try JSONDecoder().decode([SearchResult].self, from: data)
        }
        let rawHTTP = try loadOracle("oracle-http-code.raw.json")
        let rawCLI = try loadOracle("oracle-cli-code.raw.json")
        try requireStartup(rawHTTP.count == config.expected_count && rawCLI.count == config.expected_count, "Unexpected oracle count")
        try requireStartup(rawHTTP == rawCLI, "Exact TopK gate: CLI and HTTP must match in all six fields and original order")
        func guiOrder(_ values: [SearchResult]) -> [SearchResult] {
            values.sorted {
                if $0.score != $1.score { return $0.score < $1.score }
                return $0.path.localizedCaseInsensitiveCompare($1.path) == .orderedAscending
            }
        }
        let expectedHTTP = guiOrder(rawHTTP)
        let expectedCLI = guiOrder(rawCLI)
        let context = StartupTransport(config)
        StartupProtocol.transport = context
        let settings = URLSessionConfiguration.ephemeral
        settings.protocolClasses = [StartupProtocol.self]
        let session = URLSession(configuration: settings)
        let model = SearchViewModel(cli: MacEveryCLI(executableURL: URL(fileURLWithPath: config.wrapper)),
                                    searchSession: session, managesChildProcesses: false, searchDebounceDelay: 0.01)
        var cases: [[String: Any]] = []
        var control: [String: Any] = ["pause_service": false, "hold_cli": false, "phase": "initial"]
        func writeControl() throws {
            try JSONSerialization.data(withJSONObject: control, options: [.sortedKeys])
                .write(to: root.appendingPathComponent("control.json"), options: .atomic)
        }
        func entries(_ filename: String) -> [[String: Any]] {
            let text = (try? String(contentsOf: root.appendingPathComponent(filename), encoding: .utf8)) ?? ""
            return text.split(separator: "\n").compactMap {
                (try? JSONSerialization.jsonObject(with: Data($0.utf8))) as? [String: Any]
            }
        }
        func cliSearches() -> [[String: Any]] { entries("cli.events.jsonl").filter { $0["kind"] as? String == "child_start" && $0["role"] as? String == "search" } }
        func saveCase(_ name: String, _ details: [String: Any] = [:]) throws {
            var value = details
            value.merge(["name": name, "status": "PASS"]) { _, new in new }
            cases.append(value)
            try JSONSerialization.data(withJSONObject: ["cases": cases], options: [.prettyPrinted, .sortedKeys])
                .write(to: root.appendingPathComponent("model-summary.json"), options: .atomic)
            print("PASS: \(name)")
        }
        func pause(_ value: Bool, phase: String, holdCLI: Bool = false) throws {
            control = ["pause_service": value, "hold_cli": holdCLI, "phase": phase]
            try writeControl()
            try startupPump(until: {
                entries("control.events.jsonl").contains { $0["kind"] as? String == (value ? "service_paused" : "service_resumed") && $0["phase"] as? String == phase }
            }, timeout: 3)
        }
        var disallowResults = false
        var stalePublications = 0
        let observations = model.$results.sink { if disallowResults && !$0.isEmpty { stalePublications += 1 } }
        defer {
            observations.cancel()
            model.query = ""
            control["pause_service"] = false
            control["hold_cli"] = false
            try? writeControl()
            session.invalidateAndCancel()
            context.session.invalidateAndCancel()
        }

        context.setPhase("cold_immediate_code")
        let coldStart = ProcessInfo.processInfo.systemUptime
        let coldCLI = cliSearches().count
        model.bootstrapIfNeeded()
        model.query = config.query // Deliberately no readiness wait.
        try startupPump(until: { context.countSearch(config.query) > 0 }, timeout: 5)
        try startupPump(until: { !model.isSearching && model.results.count == config.expected_count }, timeout: config.timeout)
        let coldFallbacks = Array(cliSearches().dropFirst(coldCLI))
        let coldRoute: String
        if coldFallbacks.isEmpty {
            try requireStartup(context.deliveredHTTP200(phase: "cold_immediate_code", query: config.query), "Cold HTTP route lacks a successful delivered response")
            try requireStartup(model.results == expectedHTTP, "Cold HTTP result differs from ordered memory HTTP oracle")
            coldRoute = "http-memory"
        } else {
            try requireStartup(coldFallbacks.count == 1 && !context.deliveredHTTP200(phase: "cold_immediate_code", query: config.query), "Unexpected cold CLI fallback after successful HTTP or multiple CLI children")
            let child = coldFallbacks[0]["child_pid"] as! Int
            try requireStartup(entries("cli.events.jsonl").contains { $0["kind"] as? String == "child_reaped" && $0["child_pid"] as? Int == child && $0["returncode"] as? Int == 0 && $0["cancelled"] as? Bool == false }, "Cold CLI route lacks its successful wait4 receipt")
            try requireStartup(model.results == expectedCLI, "Cold CLI fallback differs from ordered SQLite CLI oracle")
            coldRoute = "cli-sqlite"
        }
        try saveCase("cold_immediate_code", ["count": model.results.count, "oracle_route": coldRoute,
                                            "wall_seconds": ProcessInfo.processInfo.systemUptime - coldStart])
        try startupPump(until: { model.status != nil && model.serviceStatus?.ready == true && model.serviceStatus?.stale == false }, timeout: config.timeout)

        model.query = ""
        context.setPhase("clear_inflight", deliveryDelay: 0.5)
        let beforeClear = context.countSearch(config.query)
        let beforeClearCLI = cliSearches().count
        disallowResults = true
        model.query = config.query
        try startupPump(until: { context.countSearch(config.query) > beforeClear }, timeout: 3)
        model.query = ""
        startupPump(for: 0.7)
        try requireStartup(model.results.isEmpty && !model.isSearching && stalePublications == 0, "Clear query refilled stale results")
        try requireStartup(cliSearches().count == beforeClearCLI, "Clear query launched a CLI fallback")
        try saveCase("clear_inflight", ["stale_publications": stalePublications])

        context.setPhase("rapid_latest_only", deliveryDelay: 0.5)
        let rapidCLI = cliSearches().count
        for query in [config.query, "kind:file " + config.query, config.nohit] {
            let count = context.countSearch(query)
            model.query = query
            try startupPump(until: { context.countSearch(query) > count }, timeout: 3)
        }
        try startupPump(until: { !model.isSearching }, timeout: config.timeout)
        startupPump(for: 0.7)
        try requireStartup(model.query == config.nohit && model.results.isEmpty && stalePublications == 0, "Rapid typing published an obsolete generation")
        try requireStartup(cliSearches().count == rapidCLI, "Rapid cancellation launched CLI fallback")
        try saveCase("rapid_latest_only", ["stale_publications": stalePublications])
        disallowResults = false
        context.setPhase("warm_code")
        let warmCLI = cliSearches().count
        model.query = config.query
        try startupPump(until: { !model.isSearching && model.results.count == expectedHTTP.count }, timeout: config.timeout)
        try requireStartup(cliSearches().count == warmCLI && context.deliveredHTTP200(phase: "warm_code", query: config.query), "Warm query must succeed through HTTP without CLI fallback")
        try requireStartup(model.results == expectedHTTP, "Warm HTTP query differs from ordered memory HTTP oracle")
        try saveCase("warm_code", ["count": model.results.count, "oracle_route": "http-memory"])

        for cancel in [false, true] {
            let phase = cancel ? "transport_timeout_cli_cancel" : "transport_timeout_cli_success"
            model.query = ""
            context.setPhase(phase)
            try pause(true, phase: phase, holdCLI: cancel)
            let before = cliSearches().count
            let started = ProcessInfo.processInfo.systemUptime
            model.query = config.query
            try startupPump(until: { cliSearches().count > before }, timeout: 40)
            let wait = ProcessInfo.processInfo.systemUptime - started
            try requireStartup(wait >= 15, "Transport did not exercise the production 15-second timeout")
            let child = cliSearches().last!["child_pid"] as! Int
            if cancel {
                try startupPump(until: { entries("cli.events.jsonl").contains { $0["kind"] as? String == "child_held" && $0["child_pid"] as? Int == child } }, timeout: 3)
                model.query = ""
                try startupPump(until: { entries("cli.events.jsonl").contains { $0["kind"] as? String == "child_reaped" && $0["child_pid"] as? Int == child && $0["cancelled"] as? Bool == true } }, timeout: 3)
            } else {
                try startupPump(until: { !model.isSearching && model.results.count == expectedCLI.count }, timeout: 18)
                try requireStartup(model.results == expectedCLI, "Real CLI timeout fallback differs from ordered SQLite CLI oracle")
            }
            try pause(false, phase: phase)
            startupPump(for: 0.4)
            try requireStartup(cliSearches().count == before + 1, "Timeout launched multiple CLI searches")
            if cancel { try requireStartup(model.results.isEmpty && !model.isSearching, "Cancelled timeout CLI refilled results") }
            try saveCase(phase, ["transport_wait_seconds": wait, "owned_child_pid": child,
                                 "cancelled": cancel, "count": model.results.count, "oracle_route": "cli-sqlite"])
        }
        model.query = ""
        try requireStartup(!model.isSearching && model.results.isEmpty, "Final model cleanup failed")
        try saveCase("final_clear")
    }
}
