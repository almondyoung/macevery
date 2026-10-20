import Darwin
import Foundation

// Compile together with MacEveryApp.swift and -D MACEVERY_TESTING. No listener,
// production CLI, user database, preferences mutation, or native App is used.
private struct ResourceCheckFailure: Error, CustomStringConvertible {
    let description: String
}

private func require(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    if !condition() { throw ResourceCheckFailure(description: message) }
}

private func pump(until condition: () -> Bool, timeout: TimeInterval = 4, label: String = "resource check") throws {
    let end = Date().addingTimeInterval(timeout)
    while !condition() && Date() < end {
        autoreleasepool {
            _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
        }
    }
    try require(condition(), "Timed out waiting for \(label)")
}

private func pump(for seconds: TimeInterval) {
    let end = Date().addingTimeInterval(seconds)
    while Date() < end {
        autoreleasepool {
            _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
        }
    }
}

private func rows(_ query: String) -> [SearchResult] {
    let kinds: [String] = ["file", "dir", "app", "symlink"]
    return (0..<300).map { (i: Int) -> SearchResult in
        let basename = "\(i)-文件.swift"
        let path = "/round8/\(query)/\(basename)"
        let size: Int64? = i % 7 == 0 ? nil : Int64(i * 4096)
        let mtime: Int64? = i % 11 == 0 ? nil : Int64(1_700_000_000 + i * 3600)
        return SearchResult(path: path, basename: basename, kind: kinds[i % kinds.count],
                            size: size, mtime: mtime, score: Int64(i))
    }
}

private final class ResourceProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    private static var requests: [String: Int] = [:]
    private let stateLock = NSLock()
    private var stopped = false
    private var work: DispatchWorkItem?

    static func count(_ query: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return requests[query, default: 0]
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url, url.host == "127.0.0.1", url.port == 17649 else {
            client?.urlProtocol(self, didFailWithError: URLError(.unsupportedURL))
            return
        }
        let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "q" })?.value ?? ""
        Self.lock.lock()
        Self.requests[query, default: 0] += 1
        Self.lock.unlock()
        let data: Data
        do {
            data = try JSONEncoder().encode(rows(query))
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
            return
        }
        let item = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.stateLock.lock()
            let stopped = self.stopped
            self.stateLock.unlock()
            guard !stopped else { return }
            let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)!
            self.client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            self.client?.urlProtocol(self, didLoad: data)
            self.client?.urlProtocolDidFinishLoading(self)
        }
        stateLock.lock()
        work = item
        stateLock.unlock()
        DispatchQueue.global().asyncAfter(deadline: .now() + (query.hasPrefix("old") ? 0.2 : 0.005), execute: item)
    }

    override func stopLoading() {
        stateLock.lock()
        stopped = true
        work?.cancel()
        stateLock.unlock()
    }
}

private func isolatedSession() -> URLSession {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [ResourceProtocol.self]
    config.urlCache = nil
    return URLSession(configuration: config)
}

private let fixtureEnvironmentKey = "MACEVERY_GUI_RESOURCE_FIXTURE"

private func appendEvent(_ text: String, root: URL) throws {
    let path = root.appendingPathComponent("events.txt").path
    let fd = path.withCString { Darwin.open($0, O_WRONLY | O_CREAT | O_APPEND, mode_t(0o600)) }
    guard fd >= 0 else { throw ResourceCheckFailure(description: "Cannot open fixture ledger") }
    defer { Darwin.close(fd) }
    let bytes = Array((text + "\n").utf8)
    try bytes.withUnsafeBytes { buffer in
        var offset = 0
        while offset < buffer.count {
            let count = Darwin.write(fd, buffer.baseAddress!.advanced(by: offset), buffer.count - offset)
            guard count > 0 else { throw ResourceCheckFailure(description: "Cannot append fixture ledger") }
            offset += count
        }
    }
}

// This executable doubles as its own tiny fake CLI. Unknown CLI commands fail
// closed rather than launching the normal checks recursively.
private func fixtureCommand(_ arguments: [String]) -> Int32 {
    guard let raw = getenv(fixtureEnvironmentKey) else { return 64 }
    let root = URL(fileURLWithPath: String(cString: raw), isDirectory: true)
    do {
        switch arguments.first {
        case "status":
            let snapshot = try Data(contentsOf: root.appendingPathComponent("status.json"))
            try appendEvent("status-start \(getpid())", root: root)
            let deadline = Date().addingTimeInterval(8)
            while !FileManager.default.fileExists(atPath: root.appendingPathComponent("release").path) {
                guard Date() < deadline else { return 70 }
                usleep(5_000)
            }
            try appendEvent("status-finish \(getpid())", root: root)
            FileHandle.standardOutput.write(snapshot)
            return 0
        case "index":
            try appendEvent("index", root: root)
            FileHandle.standardOutput.write(Data("fixture indexed\n".utf8))
            return 0
        case "watch":
            try appendEvent("watch-start \(getpid())", root: root)
            FileHandle.standardError.write(Data("fixture final watch error before fast exit\n".utf8))
            try appendEvent("watch-final-written \(getpid())", root: root)
            return 73
        default:
            FileHandle.standardError.write(Data("Unexpected isolated fixture command\n".utf8))
            return 64
        }
    } catch {
        FileHandle.standardError.write(Data("Isolated fixture I/O failed\n".utf8))
        return 74
    }
}

private final class Fixture {
    let root: URL
    private let oldEnvironment: String?

    init(parent: URL, name: String, files: Int64 = 1, suffix: String = "first") throws {
        root = parent.appendingPathComponent(name, isDirectory: true)
        oldEnvironment = getenv(fixtureEnvironmentKey).map { String(cString: $0) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        setenv(fixtureEnvironmentKey, root.path, 1)
        try payload(files: files, suffix: suffix)
    }

    func payload(files: Int64, suffix: String) throws {
        let value: [String: Any] = ["db_path": "/round8/index.sqlite", "files": files, "dirs": 0,
            "apps": 0, "symlinks": 0, "others": 0, "last_indexed_at": NSNull(),
            "roots": ["/round8/root-\(suffix)"], "excludes": [".git"]]
        try JSONSerialization.data(withJSONObject: value).write(to: root.appendingPathComponent("status.json"), options: .atomic)
    }

    func release() { try? Data().write(to: root.appendingPathComponent("release")) }
    func events() -> [String] {
        ((try? String(contentsOf: root.appendingPathComponent("events.txt"), encoding: .utf8)) ?? "")
            .split(separator: "\n").map(String.init)
    }
    func statusStarts() -> Int { events().filter { $0.hasPrefix("status-start ") }.count }
    func cli() -> MacEveryCLI {
        MacEveryCLI(executableURL: URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL)
    }

    deinit {
        // Keep fixture evidence and release any blocked own child on failure.
        release()
        if let oldEnvironment { setenv(fixtureEnvironmentKey, oldEnvironment, 1) }
        else { unsetenv(fixtureEnvironmentKey) }
    }
}

private func model(_ cli: MacEveryCLI, session: URLSession) -> SearchViewModel {
    SearchViewModel(cli: cli, searchSession: session, managesChildProcesses: false, searchDebounceDelay: 0)
}

private final class ConcurrentFailureFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var failed = false
    func mark() { lock.lock(); failed = true; lock.unlock() }
    var value: Bool { lock.lock(); defer { lock.unlock() }; return failed }
}

private final class OneShotReadGate: @unchecked Sendable {
    private let lock = NSLock()
    private let release = DispatchSemaphore(value: 0)
    private var didEnter = false
    private var didTimeOut = false
    private var enteredOnMain = false
    var entered: Bool { lock.lock(); defer { lock.unlock() }; return didEnter }
    var timedOut: Bool { lock.lock(); defer { lock.unlock() }; return didTimeOut }
    var onMain: Bool { lock.lock(); defer { lock.unlock() }; return enteredOnMain }
    func pauseOnce() {
        lock.lock()
        guard !didEnter else { lock.unlock(); return }
        didEnter = true
        enteredOnMain = Thread.isMainThread
        lock.unlock()
        if release.wait(timeout: .now() + 4) != .success {
            lock.lock(); didTimeOut = true; lock.unlock()
        }
    }
    func open() { release.signal() }
}

private func formatterChecks() throws {
    let locales = [Locale(identifier: "en_US"), Locale(identifier: "en_GB"), Locale(identifier: "zh_CN")]
    let zones = [TimeZone(secondsFromGMT: 0)!, TimeZone(identifier: "America/Los_Angeles")!, TimeZone(identifier: "Asia/Shanghai")!]
    var context = DisplayFormattingEnvironment.current
    let formatters = MacEveryDisplayFormatters(environment: { context })
    for i in 0..<locales.count {
        var calendar = Calendar(identifier: i == 2 ? .buddhist : .gregorian)
        calendar.timeZone = zones[i]
        context = DisplayFormattingEnvironment(locale: locales[i], timeZone: zones[i], calendar: calendar)
        let date = DateFormatter()
        date.locale = context.locale
        date.calendar = context.calendar
        date.timeZone = context.timeZone
        date.dateStyle = .medium
        date.timeStyle = .short
        let count = NumberFormatter()
        count.locale = context.locale
        count.numberStyle = .decimal
        for row in rows("display") {
            let expected = row.mtime.map { date.string(from: Date(timeIntervalSince1970: TimeInterval($0))) } ?? "-"
            try require(formatters.date(row.mtime) == expected, "Date semantics changed for the fixed 300 rows")
            let number = row.size ?? 0
            try require(formatters.count(number) == (count.string(from: NSNumber(value: number)) ?? "\(number)"), "Count locale semantics changed")
        }
        NotificationCenter.default.post(name: NSLocale.currentLocaleDidChangeNotification, object: nil)
        NotificationCenter.default.post(name: .NSSystemTimeZoneDidChange, object: nil)
        try require(formatters.date(0) == date.string(from: Date(timeIntervalSince1970: 0)), "Notification invalidation changed display")
    }
    let legacy = DateFormatter()
    legacy.dateStyle = .medium
    legacy.timeStyle = .short
    for row in rows("current-display") {
        let expected = row.mtime.map { legacy.string(from: Date(timeIntervalSince1970: TimeInterval($0))) } ?? "-"
        try require(MacEveryDisplayFormatters.shared.date(row.mtime) == expected, "Default locale/calendar/time-zone semantics changed")
    }
    let frozen = context
    let concurrent = MacEveryDisplayFormatters(environment: { frozen })
    let expected = rows("threads").map { concurrent.date($0.mtime) }
    let failure = ConcurrentFailureFlag()
    let group = DispatchGroup()
    for _ in 0..<4 {
        group.enter()
        DispatchQueue.global().async {
            for (index, row) in rows("threads").enumerated() {
                if concurrent.date(row.mtime) != expected[index] {
                    failure.mark()
                }
            }
            group.leave()
        }
    }
    try require(group.wait(timeout: .now() + 4) == .success, "Concurrent formatter calls did not finish")
    try require(!failure.value, "Formatter state raced across callers")
    var temporary: MacEveryDisplayFormatters? = MacEveryDisplayFormatters()
    weak var weakFormatter = temporary
    temporary = nil
    try require(weakFormatter == nil, "Notification observers retained formatter instance")
    print("PASS: 300-row date/count semantics, locale/time-zone/calendar refresh, concurrency, formatter release")
}

private func resultChecks(_ parent: URL) throws {
    let fixture = try Fixture(parent: parent, name: "results")
    let session = isolatedSession()
    defer { session.invalidateAndCancel(); fixture.release() }
    var viewModel: SearchViewModel? = model(fixture.cli(), session: session)
    viewModel!.query = "all"
    try pump(until: { viewModel?.results.count == 300 && viewModel?.isSearching == false })
    try require(viewModel!.results == rows("all"), "Fixed 300 complete fields/order changed")
    viewModel!.query = "old-replace"
    try pump(until: { ResourceProtocol.count("old-replace") == 1 })
    viewModel!.query = "latest"
    try pump(until: { viewModel?.results == rows("latest") && viewModel?.isSearching == false })
    viewModel!.query = "old-clear"
    try pump(until: { ResourceProtocol.count("old-clear") == 1 })
    viewModel!.query = ""
    pump(for: 0.3)
    try require(viewModel!.results.isEmpty && viewModel!.selectedPath == nil && !viewModel!.isSearching,
                "Cleared query was refilled by an old response")
    try require(fixture.events().isEmpty, "Successful isolated HTTP unexpectedly launched a CLI")
    weak var weakModel = viewModel
    viewModel = nil
    try require(weakModel == nil, "Completed/cancelled result work retained the model")
    print("PASS: fixed 300 ordered fields, rapid replacement, clear without refill, model release")
}

private func statusChecks(_ parent: URL) throws {
    do {
        let fixture = try Fixture(parent: parent, name: "status-coalesced")
        let session = isolatedSession()
        defer { session.invalidateAndCancel(); fixture.release() }
        let viewModel = model(fixture.cli(), session: session)
        viewModel.refreshStatus()
        try pump(until: { fixture.statusStarts() == 1 })
        for _ in 0..<50 { viewModel.refreshStatus() }
        pump(for: 0.04)
        try require(fixture.statusStarts() == 1, "Refresh burst overlapped CLI status children")
        try fixture.payload(files: 2, suffix: "latest")
        fixture.release()
        try pump(until: { fixture.statusStarts() == 2 && viewModel.status?.files == 2 })
        pump(for: 0.05)
        try require(fixture.statusStarts() == 2, "Refresh burst did not coalesce to one follow-up")
        var active = 0
        var maximum = 0
        for event in fixture.events() {
            if event.hasPrefix("status-start ") { active += 1; maximum = max(maximum, active) }
            if event.hasPrefix("status-finish ") { active -= 1 }
        }
        try require(maximum == 1 && active == 0, "Status CLI lifecycle was not single-flight/reaped")
    }
    do {
        let fixture = try Fixture(parent: parent, name: "status-epoch", files: 3, suffix: "old")
        let session = isolatedSession()
        defer { session.invalidateAndCancel(); fixture.release() }
        let viewModel = model(fixture.cli(), session: session)
        viewModel.refreshStatus()
        try pump(until: { fixture.statusStarts() == 1 })
        try fixture.payload(files: 7, suffix: "rebuilt")
        viewModel.rebuildIndex()
        try pump(until: { fixture.events().contains("index") })
        fixture.release()
        try pump(until: { !viewModel.isIndexing && viewModel.status?.files == 7 })
        pump(for: 0.05)
        try require(viewModel.settings.roots == ["/round8/root-rebuilt"] && viewModel.status?.files == 7,
                    "An old metadata epoch replaced rebuilt counts/configuration")
    }
    do {
        let fixture = try Fixture(parent: parent, name: "status-release")
        let session = isolatedSession()
        defer { session.invalidateAndCancel(); fixture.release() }
        var cli: MacEveryCLI? = fixture.cli()
        var viewModel: SearchViewModel? = model(cli!, session: session)
        weak var weakModel = viewModel
        weak var weakCLI = cli
        viewModel!.refreshStatus()
        try pump(until: { fixture.statusStarts() == 1 })
        viewModel = nil
        cli = nil
        try pump(until: { weakModel == nil && weakCLI == nil })
        try require(!FileManager.default.fileExists(atPath: fixture.root.appendingPathComponent("release").path),
                    "Blocked status release check accidentally opened its gate")
    }
    print("PASS: refresh burst single-flight/latest follow-up, metadata epoch, blocked CLI/model release")
}

private func messageChecks(_ parent: URL) throws {
    var messages: [String] = []
    var onMain = true
    let relay = WatchMessageRelay(byteLimit: 4096, delay: 0) { text in
        onMain = onMain && Thread.isMainThread
        messages.append(text)
    }
    for i in 0..<10_000 { relay.receive(Data("line-\(i)".utf8)) }
    try require(messages.isEmpty, "Message relay delivered synchronously on producer path")
    try pump(until: { messages.count == 1 })
    try require(messages == ["line-9999"] && onMain, "Stderr burst was not latest-only/main-thread coalesced")
    relay.receive(Data((String(repeating: "界🚀", count: 10_000) + "最新").utf8))
    try pump(until: { messages.count == 2 })
    try require(messages[1].utf8.count <= 4096 && messages[1].hasSuffix("最新"), "Unicode stderr was not bounded safely")
    relay.invalidate()
    relay.receive(Data("late".utf8))
    pump(for: 0.03)
    try require(messages.count == 2, "Invalidated source delivered a stale message")
    var finalMessages: [String] = []
    let fastExit = WatchMessageRelay(delay: 1) { finalMessages.append($0) }
    fastExit.receive(Data("final error before exit".utf8))
    fastExit.finishOnMain()
    try require(finalMessages == ["final error before exit"], "Fast watcher exit lost its pending error")
    fastExit.receive(Data("after exit".utf8))
    pump(for: 0.03)
    try require(finalMessages.count == 1, "Finished source delivered more than its final message")

    var temporary: WatchMessageRelay? = WatchMessageRelay(delay: 0) { _ in messages.append("retained") }
    weak var weakRelay = temporary
    temporary!.receive(Data("queued".utf8))
    temporary = nil
    try require(weakRelay == nil, "Queued delivery retained relay object")
    pump(for: 0.03)
    try require(!messages.contains("retained"), "Released relay still delivered")

    let fixture = try Fixture(parent: parent, name: "watch-identity")
    let session = isolatedSession()
    defer { session.invalidateAndCancel(); fixture.release() }
    var viewModel: SearchViewModel? = model(fixture.cli(), session: session)
    let old = viewModel!.replaceWatchMessageSourceForTesting()
    old.receive(Data("old source".utf8))
    let current = viewModel!.replaceWatchMessageSourceForTesting()
    current.receive(Data("current source".utf8))
    try pump(until: { viewModel?.message == "current source" })
    old.receive(Data("late old source".utf8))
    pump(for: 0.15)
    try require(viewModel?.message == "current source", "Old watch identity replaced current status")
    weak var weakModel = viewModel
    current.receive(Data("pending on released model".utf8))
    viewModel = nil
    try require(weakModel == nil, "Watch message callback retained model")
    pump(for: 0.15)
    print("PASS: 10k stderr burst, 4KiB UTF-8 bound, one latest delivery, source identity and object release")
}

private func fastWatchPipeChecks(_ parent: URL) throws {
    let fixture = try Fixture(parent: parent, name: "watch-fast-exit-pipe")
    let session = isolatedSession()
    let gate = OneShotReadGate()
    var viewModel: SearchViewModel? = SearchViewModel(cli: fixture.cli(), searchSession: session,
                                                     managesChildProcesses: true)
    defer {
        gate.open()
        viewModel?.stopWatcher()
        session.invalidateAndCancel()
    }
    let statusData = try Data(contentsOf: fixture.root.appendingPathComponent("status.json"))
    viewModel!.status = try JSONDecoder().decode(IndexStatus.self, from: statusData)
    viewModel!.watchStderrReadGateForTesting = { gate.pauseOnce() }
    var terminated = false
    viewModel!.watchTerminationObserverForTesting = {
        terminated = true
        try? appendEvent("watch-termination-main", root: fixture.root)
    }
    // Launch only this executable's fake watch command. No bootstrap, status,
    // service, listener or production CLI/database is involved.
    autoreleasepool { viewModel!.startWatcherIfNeeded() }
    weak var weakProcess = viewModel!.watchProcessForTesting
    weak var weakPipe = viewModel!.watchStderrForTesting
    weak var weakRelay = viewModel!.watchMessageRelayForTesting
    defer {
        try? appendEvent("check-finished entered=\(gate.entered) gateMain=\(gate.onMain) gateTimeout=\(gate.timedOut) terminated=\(terminated) process=\(weakProcess != nil) pipe=\(weakPipe != nil) relay=\(weakRelay != nil) message=\(viewModel?.message ?? "released")", root: fixture.root)
    }
    try require(weakProcess != nil && weakRelay != nil, "Fake watch did not start")
    try pump(until: { gate.entered && terminated }, timeout: 3, label: "pipe gate and process termination")
    try require(fixture.events().contains { $0.hasPrefix("watch-final-written ") },
                "Fake child did not write its final stderr before exit")
    try require(viewModel?.message != "fixture final watch error before fast exit",
                "Read gate failed to exercise termination before stderr delivery")
    gate.open()
    try pump(until: { viewModel?.message == "fixture final watch error before fast exit" }, label: "final real-pipe stderr")
    try appendEvent("check-final-stderr", root: fixture.root)
    try require(viewModel!.watchProcessForTesting == nil && viewModel!.watchStderrForTesting == nil
                && viewModel!.watchMessageRelayForTesting == nil, "Model retained finished watch resources")
    try pump(until: { weakProcess == nil && weakPipe == nil && weakRelay == nil }, label: "real watch object release")
    try require(!gate.timedOut, "Fast-exit pipe ordering depended on gate timeout")
    weak var weakModel = viewModel
    viewModel = nil
    try require(weakModel == nil, "Completed fake watch retained the model")
    print("PASS: real fake-CLI fast exit before pipe read, final stderr preserved, process/pipe/relay released")
}

@main
private struct GUIResourceRegressionChecks {
    static func main() {
        let arguments = Array(CommandLine.arguments.dropFirst())
        if arguments == ["--help"] {
            print("Usage: GUIResourceRegressionChecks [--output PRIVATE_DIRECTORY]")
            return
        }
        if !arguments.isEmpty && arguments.first != "--output" {
            Darwin.exit(fixtureCommand(arguments))
        }
        do {
            guard arguments.isEmpty || (arguments.count == 2 && arguments[0] == "--output") else {
                throw ResourceCheckFailure(description: "Invalid check arguments")
            }
            let parent = arguments.isEmpty
                ? FileManager.default.temporaryDirectory.appendingPathComponent("macevery-round8-gui-\(UUID().uuidString)", isDirectory: true)
                : URL(fileURLWithPath: arguments[1], isDirectory: true)
                    .appendingPathComponent("run-\(UUID().uuidString)", isDirectory: true)
            try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            print("Private fixture evidence: \(parent.path)")
            try formatterChecks()
            try resultChecks(parent)
            try statusChecks(parent)
            try messageChecks(parent)
            try fastWatchPipeChecks(parent)
            print("PASS: isolated GUI resource checks; no production service/database/App used")
        } catch {
            FileHandle.standardError.write(Data("FAIL: \(error)\n".utf8))
            Darwin.exit(1)
        }
    }
}
