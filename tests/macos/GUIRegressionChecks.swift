import Foundation

private struct CheckFailure: Error, CustomStringConvertible {
    let description: String
}

private func check(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    if !condition() { throw CheckFailure(description: message) }
}

private func pump(until condition: () -> Bool, timeout: TimeInterval = 3) throws {
    let deadline = Date().addingTimeInterval(timeout)
    while !condition() && Date() < deadline {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
    }
    try check(condition(), "Timed out waiting for test condition")
}

private func pump(for interval: TimeInterval) {
    let deadline = Date().addingTimeInterval(interval)
    while Date() < deadline {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
    }
}

private final class SearchProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    private static var requests: [String: Int] = [:]
    private static var cancellations: [String: Int] = [:]
    private let stateLock = NSLock()
    private var work: DispatchWorkItem?
    private var stopped = false

    static func count(_ query: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return requests[query, default: 0]
    }

    static func cancelled(_ query: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return cancellations[query, default: 0]
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    private var query: String {
        URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "q" })?.value ?? ""
    }

    override func startLoading() {
        let query = query
        Self.lock.lock()
        Self.requests[query, default: 0] += 1
        let attempt = Self.requests[query, default: 0]
        Self.lock.unlock()
        let status: Int
        switch query {
        case "busy", "old-busy": status = 503
        case "limited": status = 429
        case "recover": status = attempt < 4 ? 503 : 200
        case "old-failure": status = 500
        default: status = query.hasPrefix("cli-") ? 500 : 200
        }
        let delay = (query == "delayed" || query == "old-failure") ? 0.3 : 0.01
        let body = "[{\"path\":\"/fixture/\(query)\",\"basename\":\"\(query)\",\"kind\":\"file\",\"score\":1}]"
        let work = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.stateLock.lock()
            let stopped = self.stopped
            self.stateLock.unlock()
            guard !stopped else { return }
            if query == "transport-cancelled" {
                // The transport cancels without the model changing its query or generation.
                self.client?.urlProtocol(self, didFailWithError: URLError(.cancelled))
                return
            }
            let response = HTTPURLResponse(url: self.request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
            self.client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            self.client?.urlProtocol(self, didLoad: Data(body.utf8))
            self.client?.urlProtocolDidFinishLoading(self)
        }
        stateLock.lock()
        self.work = work
        stateLock.unlock()
        DispatchQueue.global().asyncAfter(deadline: .now() + delay, execute: work)
    }

    override func stopLoading() {
        stateLock.lock()
        stopped = true
        work?.cancel()
        stateLock.unlock()
        Self.lock.lock()
        Self.cancellations[query, default: 0] += 1
        Self.lock.unlock()
    }
}

@main
private enum GUIRegressionChecks {
    static func main() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("macevery-gui-checks-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let executable = directory.appendingPathComponent("fake-cli.py")
        let events = directory.appendingPathComponent("events.log")
        let fixture = #"""
#!/usr/bin/python3
import atexit, json, os, signal, sys, time
log = os.path.join(os.path.dirname(__file__), 'events.log')
mode = sys.argv[1]
if mode == 'large':
    for _ in range(256):
        os.write(1, b'o' * 8192)
        os.write(2, b'e' * 8192)
elif mode == 'hang':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(10)
elif mode == 'marker':
    open(log, 'a').write('unexpected launch\n')
elif mode == 'search':
    query = sys.argv[2]
    def event(kind):
        with open(log, 'a') as stream:
            stream.write(kind + ' ' + query + '\n')
    event('start')
    atexit.register(lambda: event('end'))
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    if query.startswith('cli-slow'):
        time.sleep(0.8)
    print(json.dumps([dict(path='/fixture/' + query, basename=query, kind='file', score=1)]))
"""#
        try fixture.write(to: executable, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)
        let cli = MacEveryCLI(executableURL: executable)
        try statusChecks()
        try commandChecks(cli)
        try searchChecks(cli, events: events)
        print("PASS: GUI search generation/cancellation/backpressure/single-flight and CLI pipe/timeout/output checks")
    }

    private static func statusChecks() throws {
        let oldJSON = #"{"backend":"memory","requested_backend":"memory","records":12,"estimated_memory_bytes":10800}"#
        let oldStatus = try JSONDecoder().decode(SearchServiceStatus.self, from: Data(oldJSON.utf8))
        try check(oldStatus.actualIndexBytes == nil && !oldStatus.memoryLabel.isEmpty,
                  "Legacy service status no longer decodes")
        let newJSON = #"{"backend":"memory","requested_backend":"memory","records":12,"estimated_memory_bytes":10800,"actual_index_bytes":2048,"ready":true,"stale":false}"#
        let newStatus = try JSONDecoder().decode(SearchServiceStatus.self, from: Data(newJSON.utf8))
        try check(newStatus.actualIndexBytes == 2048 && newStatus.backendLabel == "Fastest",
                  "Packed capacity or existing performance preference did not decode")
        print("PASS: old/new service status and actual index capacity compatibility")
    }

    private static func commandChecks(_ cli: MacEveryCLI) throws {
        let output = try cli.run(["large"], timeout: 5)
        try check(output.exitCode == 0, "Large-output child failed")
        try check(output.stdout.utf8.count == 2 * 1024 * 1024, "stdout was truncated")
        try check(output.stderr.utf8.count == 2 * 1024 * 1024, "stderr was truncated")
        print("PASS: concurrently drained 2 MiB stdout and 2 MiB stderr")
        do {
            _ = try cli.run(["large"], timeout: 5, maxOutputBytes: 64 * 1024)
            throw CheckFailure(description: "Output limit did not fire")
        } catch CommandRunError.outputLimitExceeded {}
        print("PASS: output memory cap")
        let timeoutStart = ProcessInfo.processInfo.systemUptime
        do {
            _ = try cli.run(["hang"], timeout: 0.15)
            throw CheckFailure(description: "Command did not time out")
        } catch CommandRunError.timedOut {}
        try check(ProcessInfo.processInfo.systemUptime - timeoutStart < 2, "Ignoring SIGTERM prevented timeout recovery")
        print("PASS: timeout reaps a child that ignores SIGTERM")
        let cancellation = CommandCancellation()
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.15) { cancellation.cancel() }
        let cancelStart = ProcessInfo.processInfo.systemUptime
        do {
            _ = try cli.run(["hang"], cancellation: cancellation, timeout: 5)
            throw CheckFailure(description: "Command did not cancel")
        } catch CommandRunError.cancelled {}
        try check(ProcessInfo.processInfo.systemUptime - cancelStart < 2, "Cancellation waited for command timeout")
        let preCancelled = CommandCancellation()
        preCancelled.cancel()
        do {
            _ = try cli.run(["marker"], cancellation: preCancelled)
            throw CheckFailure(description: "Pre-cancelled command launched")
        } catch CommandRunError.cancelled {}
        print("PASS: running and queued command cancellation")
    }

    private static func searchChecks(_ cli: MacEveryCLI, events: URL) throws {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [SearchProtocol.self]
        let session = URLSession(configuration: config)
        defer { session.invalidateAndCancel() }
        let model = SearchViewModel(cli: cli, searchSession: session, managesChildProcesses: false, searchDebounceDelay: 0.005)
        func log() -> String { (try? String(contentsOf: events, encoding: .utf8)) ?? "" }
        try check(log().isEmpty, "Pre-cancelled fixture unexpectedly launched")

        model.query = "transport-cancelled"
        try pump(until: { SearchProtocol.count("transport-cancelled") == 1 })
        pump(for: 0.15)
        try check(model.query == "transport-cancelled", "Transport cancellation changed the user's query")
        try check(SearchProtocol.count("transport-cancelled") == 1, "Transport cancellation unexpectedly retried")
        try check(log().isEmpty, "Current-generation transport cancellation launched CLI fallback")
        let cancellationState = "CHECK: unchanged query transport-cancelled, requests=1, CLI fallback=false, isSearching=\(model.isSearching)\n"
        FileHandle.standardOutput.write(Data(cancellationState.utf8))
        try check(!model.isSearching, "Current-generation transport cancellation left isSearching=true with no active task")
        print("PASS: current-generation transport cancellation stops busy without CLI fallback")

        model.query = "delayed"
        try pump(until: { SearchProtocol.count("delayed") == 1 })
        model.query = ""
        pump(for: 0.4)
        try check(model.results.isEmpty && model.selectedPath == nil && !model.isSearching, "Cleared query refilled with old results")
        try check(SearchProtocol.cancelled("delayed") == 1, "In-flight URL task was not cancelled")
        try check(log().isEmpty, "Cancelled network request launched CLI fallback")
        print("PASS: clear query invalidates generation and cancels its URL task")

        model.query = "old-failure"
        try pump(until: { SearchProtocol.count("old-failure") == 1 })
        model.query = "latest"
        try pump(until: { model.results.first?.basename == "latest" })
        pump(for: 0.4)
        try check(model.results.first?.basename == "latest", "Old query replaced latest results")
        try check(log().isEmpty, "Superseded query launched CLI fallback")
        print("PASS: superseded service failure never launches CLI fallback")

        for query in ["busy", "limited"] {
            model.query = query
            try pump(until: { !model.isSearching && SearchProtocol.count(query) == 6 }, timeout: 6)
            try check(model.message.contains("busy"), "Exhausted backpressure did not report busy")
            try check(log().isEmpty, "503/429 bypassed service limit via CLI fallback")
        }
        let recoveryStarted = ProcessInfo.processInfo.systemUptime
        model.query = "recover"
        try pump(until: { model.results.first?.basename == "recover" })
        try check(SearchProtocol.count("recover") == 4 && log().isEmpty, "Backpressure retry recovery failed")
        try check(ProcessInfo.processInfo.systemUptime - recoveryStarted > 0.45,
                  "Recovery did not exercise a service busy for longer than 450 ms")
        model.query = "old-busy"
        try pump(until: { SearchProtocol.count("old-busy") == 1 })
        pump(for: 0.04)
        model.query = "latest-again"
        try pump(until: { model.results.first?.basename == "latest-again" })
        pump(for: 0.5)
        try check(SearchProtocol.count("old-busy") == 1, "Superseded busy query retried")
        print("PASS: five bounded 503/429 retries, recovery after 1.05s busy, and stale retry suppression")

        model.query = "cli-slow-one"
        try pump(until: { log().contains("start cli-slow-one\n") })
        model.query = "cli-slow-two"
        try pump(until: { log().contains("start cli-slow-two\n") })
        model.query = "cli-latest"
        try pump(until: { model.results.first?.basename == "cli-latest" })
        let lines = log().split(separator: "\n").map(String.init)
        try check(lines == [
            "start cli-slow-one", "end cli-slow-one",
            "start cli-slow-two", "end cli-slow-two",
            "start cli-latest", "end cli-latest"
        ], "CLI executions overlapped or stale results won: \(lines)")
        print("PASS: single-flight CLI cancels obsolete child before launching latest")

        model.query = "cli-slow-clear"
        try pump(until: { log().contains("start cli-slow-clear\n") })
        model.query = ""
        try pump(until: { log().contains("end cli-slow-clear\n") })
        pump(for: 0.1)
        try check(model.results.isEmpty && model.selectedPath == nil && !model.isSearching, "Cancelled CLI refilled cleared results")
        print("PASS: clearing an in-flight CLI reaps child without refilling results")
    }
}
