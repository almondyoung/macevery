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

// The injected session always intercepts readiness requests, including hangs,
// so bootstrap checks never contact the running App's localhost service.
private final class ReadinessProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    private static var requests: [String: Int] = [:]
    private static var replies: [String: Int] = [:]
    private static var cancellations: [String: Int] = [:]
    private let stateLock = NSLock()
    private var work: DispatchWorkItem?
    private var stopped = false

    static func count(_ directory: URL) -> Int { value(directory, metric: 0) }
    static func replied(_ directory: URL) -> Int { value(directory, metric: 1) }
    static func cancelled(_ directory: URL) -> Int { value(directory, metric: 2) }

    private static func value(_ directory: URL, metric: Int) -> Int {
        lock.lock()
        defer { lock.unlock() }
        switch metric {
        case 0: return requests[directory.path, default: 0]
        case 1: return replies[directory.path, default: 0]
        default: return cancellations[directory.path, default: 0]
        }
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let path = request.value(forHTTPHeaderField: "X-MacEvery-Fixture")!
        Self.lock.lock()
        Self.requests[path, default: 0] += 1
        Self.lock.unlock()
        let data = try! Data(contentsOf: URL(fileURLWithPath: path).appendingPathComponent("config.json"))
        let config = try! JSONSerialization.jsonObject(with: data) as! [String: Any]
        if config["service_hang"] as? Bool == true { return }
        var payload: [String: Any] = [
            "backend": config["service_backend"] as? String ?? "memory", "requested_backend": "memory",
            "records": 17, "estimated_memory_bytes": 10800
        ]
        if config["service_legacy"] as? Bool != true {
            payload["ready"] = config["service_ready"] as? Bool ?? false
            payload["stale"] = config["service_stale"] as? Bool ?? false
        }
        let body = try! JSONSerialization.data(withJSONObject: payload)
        let status = config["service_http"] as? Int ?? 200
        let work = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.stateLock.lock()
            let stopped = self.stopped
            self.stateLock.unlock()
            guard !stopped else { return }
            Self.lock.lock()
            Self.replies[path, default: 0] += 1
            Self.lock.unlock()
            let response = HTTPURLResponse(url: self.request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
            self.client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            self.client?.urlProtocol(self, didLoad: body)
            self.client?.urlProtocolDidFinishLoading(self)
        }
        stateLock.lock()
        self.work = work
        stateLock.unlock()
        DispatchQueue.global().asyncAfter(deadline: .now() + (config["service_delay"] as? Double ?? 0.002), execute: work)
    }

    override func stopLoading() {
        stateLock.lock()
        stopped = true
        work?.cancel()
        stateLock.unlock()
        let path = request.value(forHTTPHeaderField: "X-MacEvery-Fixture")!
        Self.lock.lock()
        Self.cancellations[path, default: 0] += 1
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
        try bootstrapChecks(directory)
        try commandChecks(cli)
        try searchChecks(cli, events: events)
        print("PASS: GUI search generation/cancellation/backpressure/single-flight and CLI pipe/timeout/output checks")
    }

    private static func statusChecks() throws {
        let oldJSON = #"{"backend":"memory","requested_backend":"memory","records":12,"estimated_memory_bytes":10800}"#
        let oldStatus = try JSONDecoder().decode(SearchServiceStatus.self, from: Data(oldJSON.utf8))
        try check(oldStatus.actualIndexBytes == nil && oldStatus.ready == nil && oldStatus.stale == nil && !oldStatus.memoryLabel.isEmpty,
                  "Legacy service status no longer decodes")
        let newJSON = #"{"backend":"memory","requested_backend":"memory","records":12,"estimated_memory_bytes":10800,"actual_index_bytes":2048,"ready":true,"stale":false}"#
        let newStatus = try JSONDecoder().decode(SearchServiceStatus.self, from: Data(newJSON.utf8))
        try check(newStatus.actualIndexBytes == 2048 && newStatus.backendLabel == "Fastest"
                  && newStatus.ready == true && newStatus.stale == false,
                  "Packed capacity or existing performance preference did not decode")
        print("PASS: old/new service status and actual index capacity compatibility")
    }

    private static func bootstrapChecks(_ parent: URL) throws {
        // Each fake CLI is private to this fixture. Gates make full status stay
        // pending until child-start requests and bootstrap-only UI guards are checked.
        let fixture = #"""
#!/usr/bin/python3
import json, os, sys, time
root = os.path.dirname(__file__)
mode = sys.argv[1]
with open(os.path.join(root, 'events.log'), 'a') as stream:
    stream.write(mode + '\n')
with open(os.path.join(root, 'config.json')) as stream:
    config = json.load(stream)
with open(os.path.join(root, 'events.log'), 'a') as stream:
    stream.write(mode + '-snapshot\n')
gate = 'old-status' if mode == 'status' and config.get('hold_status') else mode
while not os.path.exists(os.path.join(root, 'release-' + gate)):
    time.sleep(0.01)
if mode == 'bootstrap':
    if config.get('error') == 'bootstrap':
        sys.stderr.write('bootstrap fixture failed\n')
        sys.exit(7)
    if config.get('error') == 'decode':
        print('{"db_path":"/fixture/index.sqlite"}')
        sys.exit(0)
    print(json.dumps(dict(db_path='/fixture/index.sqlite', has_records=config['records'],
                         roots=config['roots'], excludes=['.git'], last_indexed_at=None)))
elif mode == 'status':
    if config.get('error') == 'status':
        sys.stderr.write('status fixture failed\n')
        sys.exit(8)
    counts = dict(files=7, dirs=2, apps=1, symlinks=3, others=4) if config['records'] else dict.fromkeys(['files','dirs','apps','symlinks','others'], 0)
    counts['files'] += config.get('extra_files', 0)
    print(json.dumps(dict(db_path='/fixture/index.sqlite', roots=config['roots'],
                         excludes=['.git'], last_indexed_at=None, **counts)))
elif mode == 'index':
    config.update(records=True, roots=['/fixture/initial-root'])
    with open(os.path.join(root, 'config.json'), 'w') as stream:
        json.dump(config, stream)
    print('fixture initial index complete')
else:
    sys.exit(9)
with open(os.path.join(root, 'events.log'), 'a') as stream:
    stream.write(mode + '-done\n')
"""#
        var sessions: [URLSession] = []
        defer { sessions.forEach { $0.invalidateAndCancel() } }
        func make(_ name: String, records: Bool, roots: Bool, error: String = "", timeout: TimeInterval = 2,
                  managesChildren: Bool = false) throws -> (SearchViewModel, URL) {
            let directory = parent.appendingPathComponent(name)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let executable = directory.appendingPathComponent("fake-bootstrap.py")
            try fixture.write(to: executable, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)
            let data = try JSONSerialization.data(withJSONObject: [
                "records": records, "roots": roots ? ["/fixture/root"] : [], "error": error
            ])
            try data.write(to: directory.appendingPathComponent("config.json"))
            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [ReadinessProtocol.self]
            configuration.httpAdditionalHeaders = ["X-MacEvery-Fixture": directory.path]
            let session = URLSession(configuration: configuration)
            sessions.append(session)
            return (SearchViewModel(cli: MacEveryCLI(executableURL: executable), searchSession: session,
                                    managesChildProcesses: managesChildren, startupCountsTimeout: timeout,
                                    serviceStatusRetryDelay: 0.02), directory)
        }
        func events(_ directory: URL) -> [String] {
            ((try? String(contentsOf: directory.appendingPathComponent("events.log"), encoding: .utf8)) ?? "")
                .split(separator: "\n").map(String.init)
        }
        func release(_ directory: URL, _ command: String) throws {
            try Data().write(to: directory.appendingPathComponent("release-\(command)"))
        }
        func update(_ directory: URL, _ values: [String: Any]) throws {
            let url = directory.appendingPathComponent("config.json")
            var config = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
            config.merge(values) { _, new in new }
            try JSONSerialization.data(withJSONObject: config).write(to: url, options: .atomic)
        }

        for (records, roots) in [(true, true), (true, false), (false, true)] {
            let (model, directory) = try make("availability-\(records)-\(roots)", records: records, roots: roots)
            if records && !roots {
                let oldMode = UserDefaults.standard.object(forKey: "searchPerformanceMode")
                model.searchPerformance = .lowMemory
                if let oldMode { UserDefaults.standard.set(oldMode, forKey: "searchPerformanceMode") }
                else { UserDefaults.standard.removeObject(forKey: "searchPerformanceMode") }
                try update(directory, ["service_backend": "sqlite"])
            }
            var starts: [String] = []
            model.childStartObserverForTesting = { starts.append($0) }
            model.bootstrapIfNeeded()
            model.bootstrapIfNeeded()
            try pump(until: { events(directory).contains("bootstrap") })
            try check(model.isBootstrapping && model.status == nil, "Bootstrap was not a distinct pending state")
            try check(SearchViewModel.formatOptionalCount(model.status?.total) == "—", "Pending counts were fabricated as zero")
            try release(directory, "bootstrap")
            try pump(until: { model.startupBootstrap != nil })
            if records {
                try pump(until: { ReadinessProtocol.count(directory) > 0 })
                pump(for: 0.06)
                try check(!events(directory).contains("status"), "Full counts ran before service readiness")
            } else {
                try pump(until: { events(directory).contains("status") })
            }
            try check(model.status == nil && model.hasIndexedRecords == records, "Delayed counts blocked bootstrap availability")
            let expected = (records ? ["serve"] : []) + (roots ? ["watch"] : [])
            try check(starts == expected, "Children were not requested before full status: \(starts)")
            try check(!model.isIndexing && !events(directory).contains("index"), "Existing configuration triggered an initial rebuild")
            model.bootstrapIfNeeded()
            pump(for: 0.05)
            try check(events(directory).filter { $0 == "bootstrap" }.count == 1, "Repeated onAppear duplicated bootstrap")
            let beforeRestart = starts.count
            model.restartSearchService()
            try check(starts.count == beforeRestart + (records ? 1 : 0), "Restart still depended on missing total counts")
            if records {
                try update(directory, ["service_ready": true, "service_stale": true])
                try pump(until: { model.serviceStatus?.stale == true })
                try check(!events(directory).contains("status"), "Stale service was treated as strict ready")
                try update(directory, ["service_ready": true, "service_stale": false])
                try pump(until: { events(directory).contains("status") })
            }
            try release(directory, "status")
            try pump(until: { model.status != nil })
            try check(model.status?.total == (records ? 17 : 0), "Full status did not supply real kind counts")
            try check(model.status?.files == (records ? 7 : 0) && model.status?.others == (records ? 4 : 0), "Kind counts were not preserved")
            pump(for: 0.06)
            try check(events(directory).filter { $0 == "status" }.count == 1, "Ready triggered full counts more than once")
            if records {
                let configURL = directory.appendingPathComponent("config.json")
                var config = try JSONSerialization.jsonObject(with: Data(contentsOf: configURL)) as! [String: Any]
                config["extra_files"] = 6
                try JSONSerialization.data(withJSONObject: config).write(to: configURL)
                model.refreshStatus()
                try pump(until: { model.status?.total == 23 })
                try check(model.hasIndexedRecords, "Manual full refresh lost availability")
            }
        }

        let (manual, manualDirectory) = try make("manual-counts-consume-pending", records: true, roots: true)
        manual.bootstrapIfNeeded()
        try release(manualDirectory, "bootstrap")
        try pump(until: { ReadinessProtocol.count(manualDirectory) > 0 })
        manual.refreshStatus()
        try release(manualDirectory, "status")
        try pump(until: { manual.status?.total == 17 })
        try update(manualDirectory, ["service_ready": true])
        try pump(until: { manual.serviceStatus?.ready == true })
        pump(for: 0.06)
        try check(events(manualDirectory).filter { $0 == "status" }.count == 1,
                  "Ready duplicated a manual full refresh that consumed pending counts")

        let (restarted, restartedDirectory) = try make("old-ready-after-restart", records: true, roots: false)
        restarted.bootstrapIfNeeded()
        try release(restartedDirectory, "bootstrap")
        try pump(until: { ReadinessProtocol.count(restartedDirectory) > 0 })
        try update(restartedDirectory, ["service_ready": true])
        let repliesBefore = ReadinessProtocol.replied(restartedDirectory)
        restarted.refreshSearchServiceStatus(retries: 10)
        // Let the old transport finish while the main queue cannot consume its
        // callback. A restart then makes that already queued ready reply stale.
        Thread.sleep(forTimeInterval: 0.12)
        try check(ReadinessProtocol.replied(restartedDirectory) > repliesBefore, "Old ready response was not queued")
        try update(restartedDirectory, ["service_ready": false])
        restarted.restartSearchService()
        pump(for: 0.1)
        try check(!events(restartedDirectory).contains("status"), "Old service ready consumed a restarted service's pending counts")
        try update(restartedDirectory, ["service_ready": true])
        try pump(until: { events(restartedDirectory).contains("status") })
        try release(restartedDirectory, "status")
        try pump(until: { restarted.status?.total == 17 })
        try check(events(restartedDirectory).filter { $0 == "status" }.count == 1, "Restarted service duplicated full counts")

        let (hanging, hangingDirectory) = try make("fixed-startup-deadline", records: true, roots: true, timeout: 0.2)
        try update(hangingDirectory, ["service_hang": true])
        hanging.bootstrapIfNeeded()
        try release(hangingDirectory, "bootstrap")
        try pump(until: { ReadinessProtocol.count(hangingDirectory) == 1 })
        for step in 0..<12 {
            hanging.startSearchServiceIfNeeded()
            if step == 4 { hanging.restartSearchService() }
            pump(for: 0.025)
        }
        try check(events(hangingDirectory).filter { $0 == "status" }.count == 1,
                  "Continuous starts/restart extended the fixed startup deadline")
        try check(ReadinessProtocol.count(hangingDirectory) == 2 && ReadinessProtocol.cancelled(hangingDirectory) == 2,
                  "Continuous starts duplicated readiness polls or deadline failed to cancel a hung request")
        try check(hanging.status == nil && hanging.hasIndexedRecords && !hanging.isIndexing,
                  "Deadline fallback fabricated counts or rebuilt a usable index")
        try release(hangingDirectory, "status")
        try pump(until: { hanging.status?.total == 17 })

        let fallbackCases: [(String, [String: Any])] = [
            ("legacy", ["service_legacy": true]),
            ("stale", ["service_ready": true, "service_stale": true]),
            ("http-error", ["service_http": 503])
        ]
        for (name, configuration) in fallbackCases {
            let (model, directory) = try make("fallback-\(name)", records: true, roots: true, timeout: 0.12)
            try update(directory, configuration)
            model.bootstrapIfNeeded()
            try release(directory, "bootstrap")
            try pump(until: { events(directory).contains("status") })
            try check(model.status == nil && model.hasIndexedRecords && !model.isIndexing,
                      "\(name) readiness failure invented counts or triggered initial indexing")
            try release(directory, "status")
            try pump(until: { model.status?.total == 17 })
            pump(for: 0.05)
            try check(events(directory).filter { $0 == "status" }.count == 1 && !events(directory).contains("index"),
                      "\(name) readiness fallback did not complete exact counts once")
        }

        let (exited, exitedDirectory) = try make("owned-service-exit", records: true, roots: false, managesChildren: true)
        // This fixture's 'serve' only exits: it never opens a listening socket.
        try release(exitedDirectory, "serve")
        exited.bootstrapIfNeeded()
        try release(exitedDirectory, "bootstrap")
        try pump(until: { events(exitedDirectory).contains("serve") && events(exitedDirectory).contains("status") })
        try check(exited.status == nil && !exited.isIndexing, "Child exit fabricated counts or rebuilt the index")
        try release(exitedDirectory, "status")
        try pump(until: { exited.status?.total == 17 })
        pump(for: 0.08)
        exited.stopSearchService()
        try check(events(exitedDirectory).filter { $0 == "status" }.count == 1, "Owned child exit duplicated startup counts")
        print("PASS: strict ready gates one full count, manual refresh consumes pending, restart rejects old ready, fixed deadline and failures recover counts")

        let (initial, initialDirectory) = try make("initial-empty", records: false, roots: false)
        var initialStarts: [String] = []
        initial.childStartObserverForTesting = { initialStarts.append($0) }
        initial.bootstrapIfNeeded()
        initial.bootstrapIfNeeded()
        try release(initialDirectory, "bootstrap")
        try pump(until: { initial.isIndexing && events(initialDirectory).contains("index") })
        try check(initial.startupBootstrap?.has_records == false && initialStarts.isEmpty, "Empty new DB started children before initial indexing")
        try check(!events(initialDirectory).contains("status"), "Empty initial bootstrap unnecessarily ran full status")
        initial.bootstrapIfNeeded()
        initial.refreshStatus()
        try release(initialDirectory, "status")
        try pump(until: { initial.status?.total == 0 })
        try check(initial.isIndexing && events(initialDirectory).filter { $0 == "index" }.count == 1, "Late empty status duplicated initial indexing")
        try release(initialDirectory, "index")
        try pump(until: { !initial.isIndexing && initial.status?.total == 17 })
        try check(initialStarts == ["serve", "watch"], "Completed initial index did not start its children")
        try check(events(initialDirectory).filter { $0 == "bootstrap" }.count == 1 && events(initialDirectory).filter { $0 == "index" }.count == 1,
                  "Initial bootstrap/index were not single-flight")
        print("PASS: four bootstrap availability/root cases, delayed counts, child guards, and one initial rebuild")

        for oldRead in ["bootstrap", "status"] {
            let (model, directory) = try make("late-\(oldRead)-after-rebuild", records: false, roots: false)
            let configURL = directory.appendingPathComponent("config.json")
            if oldRead == "status" {
                var config = try JSONSerialization.jsonObject(with: Data(contentsOf: configURL)) as! [String: Any]
                config["hold_status"] = true
                try JSONSerialization.data(withJSONObject: config).write(to: configURL)
                model.refreshStatus()
            } else {
                model.bootstrapIfNeeded()
            }
            try pump(until: { events(directory).contains("\(oldRead)-snapshot") })
            // The old command now owns an empty snapshot. Finish an explicit
            // rebuild and its fresh status before allowing that old reply out.
            var config = try JSONSerialization.jsonObject(with: Data(contentsOf: configURL)) as! [String: Any]
            config.removeValue(forKey: "hold_status")
            try JSONSerialization.data(withJSONObject: config).write(to: configURL)
            model.rebuildIndex()
            try release(directory, "index")
            try release(directory, "status")
            try pump(until: { !model.isIndexing && model.status?.total == 17 })
            try release(directory, oldRead == "status" ? "old-status" : "bootstrap")
            try pump(until: { events(directory).contains("\(oldRead)-done") })
            pump(for: 0.2)
            try check(model.status?.total == 17 && model.settings.roots == ["/fixture/initial-root"],
                      "Old \(oldRead) overwrote completed rebuild metadata")
            try check(!model.isIndexing && events(directory).filter { $0 == "index" }.count == 1,
                      "Old \(oldRead) triggered another initial rebuild after completion")
        }
        print("PASS: old bootstrap/full-status snapshots cannot replace a completed rebuild or index twice")

        for error in ["bootstrap", "decode", "status"] {
            let (model, directory) = try make("error-\(error)", records: true, roots: true, error: error)
            var starts: [String] = []
            model.childStartObserverForTesting = { starts.append($0) }
            model.bootstrapIfNeeded()
            try release(directory, "bootstrap")
            if error == "status" {
                try pump(until: { ReadinessProtocol.count(directory) > 0 })
                try update(directory, ["service_ready": true])
                try pump(until: { events(directory).contains("status") })
                try release(directory, "status")
                try pump(until: { model.message.contains("status fixture failed") })
                try check(model.startupBootstrap != nil && model.hasIndexedRecords && starts == ["serve", "watch"],
                          "Full-count failure invalidated successful bootstrap")
            } else {
                try pump(until: { !model.isBootstrapping && model.message.contains("failed") })
                try check(model.startupBootstrap == nil && starts.isEmpty && !events(directory).contains("status"),
                          "Failed bootstrap was treated as a usable empty index")
            }
            try check(model.status == nil && !model.isIndexing && !events(directory).contains("index"), "Bootstrap/count error triggered a rebuild or invented counts")
        }
        print("PASS: bootstrap exit/decode and full-count failures preserve errors without fake counts or rebuild")
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
