import AppKit
import Carbon
import Darwin
import Foundation
import Security
import SwiftUI

struct SearchResult: Codable, Identifiable, Hashable {
    let path: String
    let basename: String
    let kind: String
    let size: Int64?
    let mtime: Int64?
    let score: Int64

    var id: String { path }
}

struct IndexStatus: Codable {
    let db_path: String
    let files: Int64
    let dirs: Int64
    let apps: Int64
    let symlinks: Int64
    let others: Int64
    let last_indexed_at: Int64?
    let roots: [String]
    let excludes: [String]

    var total: Int64 { files + dirs + apps + symlinks + others }
}

struct IndexBootstrap: Codable {
    let db_path: String
    let has_records: Bool
    let roots: [String]
    let excludes: [String]
    let last_indexed_at: Int64?
}

struct SearchServiceStatus: Codable {
    let backend: String
    let requestedBackend: String
    let records: Int64
    let estimatedMemoryBytes: UInt64
    var actualIndexBytes: UInt64? = nil
    var ready: Bool? = nil
    var stale: Bool? = nil

    enum CodingKeys: String, CodingKey {
        case backend
        case requestedBackend = "requested_backend"
        case records
        case estimatedMemoryBytes = "estimated_memory_bytes"
        case actualIndexBytes = "actual_index_bytes"
        case ready
        case stale
    }

    var backendLabel: String {
        switch backend {
        case "memory": return "Fastest"
        case "compact": return "Balanced"
        case "sqlite": return "Low Memory"
        default: return backend
        }
    }

    var memoryLabel: String {
        ByteCountFormatter.string(fromByteCount: Int64(actualIndexBytes ?? estimatedMemoryBytes), countStyle: .memory)
    }
}

struct CommandOutput {
    let stdout: String
    let stderr: String
    let exitCode: Int32
}

enum SearchPerformanceMode: String, CaseIterable, Identifiable {
    case automatic = "auto"
    case fastest = "memory"
    case balanced = "compact"
    case lowMemory = "sqlite"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .automatic: return "Automatic"
        case .fastest: return "Fastest"
        case .balanced: return "Balanced"
        case .lowMemory: return "Low Memory"
        }
    }

    var detail: String {
        switch self {
        case .automatic:
            return "Chooses the best mode for your index size and available memory."
        case .fastest:
            return "Loads the index into memory for the lowest latency."
        case .balanced:
            return "Keeps a compact in-memory index for local searching."
        case .lowMemory:
            return "Uses SQLite directly to keep background memory low."
        }
    }

    static func load() -> SearchPerformanceMode {
        let value = UserDefaults.standard.string(forKey: "searchPerformanceMode") ?? fastest.rawValue
        return SearchPerformanceMode(rawValue: value) ?? .automatic
    }

    func save() {
        UserDefaults.standard.set(rawValue, forKey: "searchPerformanceMode")
    }
}

enum GlobalHotKeyShortcut: String, CaseIterable, Identifiable {
    case commandShiftSpace = "command_shift_space"
    case optionSpace = "option_space"
    case controlSpace = "control_space"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .commandShiftSpace: return "Command-Shift-Space"
        case .optionSpace: return "Option-Space"
        case .controlSpace: return "Control-Space"
        }
    }

    var detail: String {
        switch self {
        case .commandShiftSpace:
            return "Recommended for most keyboards."
        case .optionSpace:
            return "Compact shortcut with fewer menu conflicts."
        case .controlSpace:
            return "Classic shortcut; may conflict with input methods."
        }
    }

    var keyCode: UInt32 {
        UInt32(kVK_Space)
    }

    var carbonModifiers: UInt32 {
        switch self {
        case .commandShiftSpace:
            return UInt32(cmdKey | shiftKey)
        case .optionSpace:
            return UInt32(optionKey)
        case .controlSpace:
            return UInt32(controlKey)
        }
    }

    static func load() -> GlobalHotKeyShortcut {
        let value = UserDefaults.standard.string(forKey: "globalHotKeyShortcut") ?? commandShiftSpace.rawValue
        return GlobalHotKeyShortcut(rawValue: value) ?? .commandShiftSpace
    }

    func save() {
        UserDefaults.standard.set(rawValue, forKey: "globalHotKeyShortcut")
    }
}

enum PermissionStatus {
    case unknown
    case likelyGranted
    case limited

    var label: String {
        switch self {
        case .unknown: return "Unknown"
        case .likelyGranted: return "Likely Granted"
        case .limited: return "Limited"
        }
    }

    var iconName: String {
        switch self {
        case .unknown: return "questionmark.circle"
        case .likelyGranted: return "checkmark.circle"
        case .limited: return "exclamationmark.triangle"
        }
    }

    var color: Color {
        switch self {
        case .unknown: return .secondary
        case .likelyGranted: return .green
        case .limited: return .orange
        }
    }
}

enum ResultSortColumn {
    case relevance
    case name
    case path
    case kind
    case size
    case modified

    var title: String {
        switch self {
        case .relevance: return "Rank"
        case .name: return "Name"
        case .path: return "Path"
        case .kind: return "Kind"
        case .size: return "Size"
        case .modified: return "Modified"
        }
    }

    var defaultAscending: Bool {
        switch self {
        case .relevance, .name, .path, .kind: return true
        case .size, .modified: return false
        }
    }
}

struct ResultColumnWidths: Equatable {
    static let spacing: CGFloat = 12
    static let horizontalPadding: CGFloat = 24
    private static let storageKey = "resultColumnWidths.v1"

    var name: CGFloat
    var path: CGFloat
    var kind: CGFloat
    var size: CGFloat
    var modified: CGFloat

    static let defaults = ResultColumnWidths(
        name: 220,
        path: 170,
        kind: 58,
        size: 86,
        modified: 150
    )

    var contentWidth: CGFloat {
        name + path + kind + size + modified + Self.spacing * 4 + Self.horizontalPadding
    }

    static func load() -> ResultColumnWidths {
        guard let stored = UserDefaults.standard.dictionary(forKey: storageKey) else {
            return defaults
        }

        var widths = defaults
        if let value = storedWidth(stored, key: "name") { widths.name = value }
        if let value = storedWidth(stored, key: "path") { widths.path = value }
        if let value = storedWidth(stored, key: "kind") { widths.kind = value }
        if let value = storedWidth(stored, key: "size") { widths.size = value }
        if let value = storedWidth(stored, key: "modified") { widths.modified = value }
        widths.clampAll()
        return widths
    }

    func save() {
        UserDefaults.standard.set(
            [
                "name": Double(name),
                "path": Double(path),
                "kind": Double(kind),
                "size": Double(size),
                "modified": Double(modified)
            ],
            forKey: Self.storageKey
        )
    }

    func width(for column: ResultSortColumn) -> CGFloat {
        switch column {
        case .relevance: return 0
        case .name: return name
        case .path: return path
        case .kind: return kind
        case .size: return size
        case .modified: return modified
        }
    }

    mutating func setWidth(_ width: CGFloat, for column: ResultSortColumn) {
        let value = Self.clamped(width, for: column)
        switch column {
        case .relevance: break
        case .name: name = value
        case .path: path = value
        case .kind: kind = value
        case .size: size = value
        case .modified: modified = value
        }
    }

    private mutating func clampAll() {
        name = Self.clamped(name, for: .name)
        path = Self.clamped(path, for: .path)
        kind = Self.clamped(kind, for: .kind)
        size = Self.clamped(size, for: .size)
        modified = Self.clamped(modified, for: .modified)
    }

    private static func clamped(_ width: CGFloat, for column: ResultSortColumn) -> CGFloat {
        min(max(width, minWidth(for: column)), maxWidth(for: column))
    }

    private static func minWidth(for column: ResultSortColumn) -> CGFloat {
        switch column {
        case .relevance: return 0
        case .name: return 120
        case .path: return 100
        case .kind: return 46
        case .size: return 64
        case .modified: return 116
        }
    }

    private static func maxWidth(for column: ResultSortColumn) -> CGFloat {
        switch column {
        case .relevance: return 0
        case .name: return 900
        case .path: return 1_400
        case .kind: return 180
        case .size: return 180
        case .modified: return 320
        }
    }

    private static func storedWidth(_ values: [String: Any], key: String) -> CGFloat? {
        if let value = values[key] as? Double {
            return CGFloat(value)
        }
        if let value = values[key] as? NSNumber {
            return CGFloat(truncating: value)
        }
        return nil
    }
}

final class GlobalHotKey {
    private var hotKeyRef: EventHotKeyRef?
    private var handlerRef: EventHandlerRef?
    private let shortcut: GlobalHotKeyShortcut
    private let callback: () -> Void
    private(set) var isRegistered = false

    init(shortcut: GlobalHotKeyShortcut, callback: @escaping () -> Void) {
        self.shortcut = shortcut
        self.callback = callback
    }

    func register() {
        guard hotKeyRef == nil else { return }

        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        let unmanagedSelf = Unmanaged.passUnretained(self).toOpaque()
        let handler: EventHandlerUPP = { _, _, userData in
            guard let userData else { return noErr }
            let owner = Unmanaged<GlobalHotKey>.fromOpaque(userData).takeUnretainedValue()
            owner.callback()
            return noErr
        }

        let installStatus = InstallEventHandler(
            GetApplicationEventTarget(),
            handler,
            1,
            &eventType,
            unmanagedSelf,
            &handlerRef
        )
        guard installStatus == noErr else { return }

        let hotKeyID = EventHotKeyID(signature: OSType(0x4d455652), id: UInt32(1))
        let registerStatus = RegisterEventHotKey(
            shortcut.keyCode,
            shortcut.carbonModifiers,
            hotKeyID,
            GetApplicationEventTarget(),
            0,
            &hotKeyRef
        )
        if registerStatus != noErr {
            unregister()
        } else {
            isRegistered = true
        }
    }

    func unregister() {
        if let hotKeyRef {
            UnregisterEventHotKey(hotKeyRef)
        }
        if let handlerRef {
            RemoveEventHandler(handlerRef)
        }
        hotKeyRef = nil
        handlerRef = nil
        isRegistered = false
    }

    deinit {
        unregister()
    }
}

enum CommandRunError: LocalizedError {
    case cancelled
    case timedOut
    case outputLimitExceeded
    case outputReadTimedOut

    var errorDescription: String? {
        switch self {
        case .cancelled: return "Command cancelled"
        case .timedOut: return "Command timed out"
        case .outputLimitExceeded: return "Command output exceeded the size limit"
        case .outputReadTimedOut: return "Command output did not close after exit"
        }
    }
}

// A cancellation may arrive before launch, while the child runs, or after exit.
// Serializing launch with cancellation ensures a cancelled queued search cannot start.
final class CommandCancellation: @unchecked Sendable {
    private let lock = NSLock()
    private var cancelled = false
    private var process: Process?

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    func start(_ process: Process) throws {
        lock.lock()
        defer { lock.unlock() }
        guard !cancelled else { throw CommandRunError.cancelled }
        try process.run()
        self.process = process
    }

    func cancel() {
        lock.lock()
        cancelled = true
        let running = process
        lock.unlock()
        if let running, running.isRunning {
            running.terminate()
        }
    }

    func detach() {
        lock.lock()
        process = nil
        lock.unlock()
    }
}

private final class CommandOutputBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private var overflowed = false
    private let limit: Int

    init(limit: Int) { self.limit = max(0, limit) }

    func append(_ chunk: Data) {
        lock.lock()
        defer { lock.unlock() }
        let remaining = max(0, limit - data.count)
        data.append(chunk.prefix(remaining))
        if chunk.count > remaining { overflowed = true }
    }

    func snapshot() -> (Data, Bool) {
        lock.lock()
        defer { lock.unlock() }
        return (data, overflowed)
    }
}

final class MacEveryCLI {
    let executableURL: URL

    init(executableURL: URL? = nil) {
        self.executableURL = executableURL ?? MacEveryCLI.findExecutable()
    }

    func run(
        _ arguments: [String],
        cancellation: CommandCancellation = CommandCancellation(),
        timeout: TimeInterval = 120,
        maxOutputBytes: Int = 8 * 1024 * 1024
    ) throws -> CommandOutput {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = arguments

        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr
        let exited = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in exited.signal() }
        try cancellation.start(process)
        defer { cancellation.detach() }

        // Drain both pipes while the child runs: waiting for exit first can
        // deadlock when either stream fills its kernel pipe buffer.
        let readers = DispatchGroup()
        let outBuffer = CommandOutputBuffer(limit: maxOutputBytes)
        let errBuffer = CommandOutputBuffer(limit: maxOutputBytes)
        for (handle, buffer) in [
            (stdout.fileHandleForReading, outBuffer),
            (stderr.fileHandleForReading, errBuffer)
        ] {
            readers.enter()
            DispatchQueue.global(qos: .utility).async {
                defer {
                    try? handle.close()
                    readers.leave()
                }
                while let chunk = try? handle.read(upToCount: 64 * 1024), !chunk.isEmpty {
                    buffer.append(chunk)
                }
            }
        }

        let deadline = ProcessInfo.processInfo.systemUptime + max(0, timeout)
        var failure: CommandRunError?
        while exited.wait(timeout: .now() + 0.05) == .timedOut {
            if cancellation.isCancelled {
                failure = .cancelled
                break
            }
            if ProcessInfo.processInfo.systemUptime >= deadline {
                failure = .timedOut
                break
            }
        }
        if failure != nil {
            if process.isRunning { process.terminate() }
            if exited.wait(timeout: .now() + 0.25) == .timedOut {
                if process.isRunning { Darwin.kill(process.processIdentifier, SIGKILL) }
                _ = exited.wait(timeout: .now() + 5)
            }
        }
        if readers.wait(timeout: .now() + 2) == .timedOut {
            try? stdout.fileHandleForReading.close()
            try? stderr.fileHandleForReading.close()
            throw failure ?? CommandRunError.outputReadTimedOut
        }
        if cancellation.isCancelled { throw CommandRunError.cancelled }
        if let failure { throw failure }
        let (outData, outOverflowed) = outBuffer.snapshot()
        let (errData, errOverflowed) = errBuffer.snapshot()
        guard !outOverflowed && !errOverflowed else { throw CommandRunError.outputLimitExceeded }
        return CommandOutput(
            stdout: String(data: outData, encoding: .utf8) ?? "",
            stderr: String(data: errData, encoding: .utf8) ?? "",
            exitCode: process.terminationStatus
        )
    }

    private static func findExecutable() -> URL {
        let fileManager = FileManager.default
        let env = ProcessInfo.processInfo.environment
        let candidates = [
            env["MACEVERY_BIN"].map(URL.init(fileURLWithPath:)),
            Optional(Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/macevery")),
            Optional(URL(fileURLWithPath: fileManager.currentDirectoryPath).appendingPathComponent("target/release/macevery")),
            Optional(URL(fileURLWithPath: fileManager.currentDirectoryPath).appendingPathComponent("target/debug/macevery"))
        ].compactMap { $0 }

        for url in candidates where fileManager.isExecutableFile(atPath: url.path) {
            return url
        }
        return URL(fileURLWithPath: "macevery")
    }
}

final class SearchViewModel: ObservableObject {
    @Published var query: String = "" {
        didSet { scheduleSearch() }
    }
    @Published var results: [SearchResult] = []
    @Published var selectedPath: String?
    @Published var status: IndexStatus?
    @Published private(set) var startupBootstrap: IndexBootstrap?
    @Published private(set) var isBootstrapping = false
    @Published var isSearching = false
    @Published var isIndexing = false
    @Published var message = "Ready"
    @Published var settings = IndexSettings.defaultSettings()
    @Published var permissionStatus: PermissionStatus = .unknown
    @Published var serviceStatus: SearchServiceStatus?
    @Published var searchPerformance = SearchPerformanceMode.load() {
        didSet {
            searchPerformance.save()
            restartSearchService()
        }
    }
    @Published var globalHotKeyShortcut = GlobalHotKeyShortcut.load() {
        didSet {
            globalHotKeyShortcut.save()
            reinstallGlobalHotKey()
        }
    }
    @Published var sortColumn: ResultSortColumn = .relevance
    @Published var sortAscending = ResultSortColumn.relevance.defaultAscending

    private let cli: MacEveryCLI
    private let searchSession: URLSession
    private let managesChildProcesses: Bool
    private let searchDebounceDelay: TimeInterval
    // Old HTTP requests may still be computing after the GUI cancels them.
    // Give the latest query up to 3.45 seconds of bounded backoff to get a slot.
    private static let serviceBusyRetryDelays: [TimeInterval] = [0.15, 0.3, 0.6, 1.2, 1.2]
    private let cliSearchQueue: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "MacEvery.latest-cli-search"
        queue.maxConcurrentOperationCount = 1
        queue.qualityOfService = .userInitiated
        return queue
    }()
    private var activeSearchTask: URLSessionDataTask?
    private var activeCLICancellation: CommandCancellation?
    private var pendingSearch: DispatchWorkItem?
    private var searchGeneration = 0
    private var watchProcess: Process?
    private var watchStdout: Pipe?
    private var watchStderr: Pipe?
    private var searchServiceProcess: Process?
    private var searchServiceStdout: Pipe?
    private var searchServiceStderr: Pipe?
    private var hotKey: GlobalHotKey?
    private var hasAttemptedInitialIndex = false
    private var hasStartedBootstrap = false
    private var indexReadGeneration = 0
    private var startupCountsGeneration: Int?
    private var startupCountsDeadline: TimeInterval?
    private var serviceStatusGeneration = 0
    private var serviceStatusTask: URLSessionDataTask?
    private var serviceStatusRetry: DispatchWorkItem?
    private var serviceStatusDeadline: DispatchWorkItem?
    private let startupCountsTimeout: TimeInterval
    private let serviceStatusRetryDelay: TimeInterval
    private var startupStatusRetryCount: Int {
        max(1, Int(ceil(startupCountsTimeout / serviceStatusRetryDelay)))
    }
#if MACEVERY_TESTING
    // Observe requests without starting a listener or watcher in model checks.
    var childStartObserverForTesting: ((String) -> Void)?
#endif
    private let authToken: String = {
        var bytes = [UInt8](repeating: 0, count: 24)
        if SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess {
            return bytes.map { String(format: "%02x", $0) }.joined()
        }
        return (UUID().uuidString + UUID().uuidString)
            .replacingOccurrences(of: "-", with: "")
            .lowercased()
    }()

    init(
        cli: MacEveryCLI = MacEveryCLI(),
        searchSession: URLSession = .shared,
        managesChildProcesses: Bool = true,
        searchDebounceDelay: TimeInterval = 0.12,
        startupCountsTimeout: TimeInterval = 60,
        serviceStatusRetryDelay: TimeInterval = 0.5
    ) {
        self.cli = cli
        self.searchSession = searchSession
        self.managesChildProcesses = managesChildProcesses
        self.searchDebounceDelay = searchDebounceDelay
        self.startupCountsTimeout = max(0.05, startupCountsTimeout)
        self.serviceStatusRetryDelay = max(0.005, serviceStatusRetryDelay)
    }

    var selectedResult: SearchResult? {
        guard let selectedPath else { return nil }
        return results.first { $0.path == selectedPath }
    }

    var hasIndexedRecords: Bool {
        status.map { $0.total > 0 } ?? startupBootstrap?.has_records ?? false
    }

    private var indexRoots: [String] {
        status?.roots ?? startupBootstrap?.roots ?? []
    }

    func bootstrapIfNeeded() {
        guard !hasStartedBootstrap else { return }
        hasStartedBootstrap = true
        isBootstrapping = true
        let generation = indexReadGeneration
        DispatchQueue.global(qos: .userInitiated).async {
            let output: CommandOutput
            do {
                output = try self.cli.run(["bootstrap", "--json"])
            } catch {
                DispatchQueue.main.async {
                    guard generation == self.indexReadGeneration else { return }
                    self.isBootstrapping = false
                    self.message = "Bootstrap failed: \(error.localizedDescription)"
                }
                return
            }
            DispatchQueue.main.async {
                guard generation == self.indexReadGeneration else { return }
                self.isBootstrapping = false
                guard output.exitCode == 0 else {
                    self.message = "Bootstrap failed: \(output.stderr.trimmingCharacters(in: .whitespacesAndNewlines))"
                    return
                }
                do {
                    let bootstrap = try JSONDecoder().decode(IndexBootstrap.self, from: Data(output.stdout.utf8))
                    self.startupBootstrap = bootstrap
                    guard !self.isIndexing, self.status == nil else { return }
                    self.applyIndexConfiguration(roots: bootstrap.roots, excludes: bootstrap.excludes)
                    if self.startInitialIndexIfNeeded(hasRecords: bootstrap.has_records, roots: bootstrap.roots) {
                        return
                    }
                    self.message = "Loading index counts..."
                    if bootstrap.has_records {
                        self.startupCountsGeneration = generation
                        self.startupCountsDeadline = ProcessInfo.processInfo.systemUptime + self.startupCountsTimeout
                        self.startSearchServiceIfNeeded()
                    }
                    if !bootstrap.roots.isEmpty {
                        self.startWatcherIfNeeded()
                    }
                    if !bootstrap.has_records {
                        self.refreshStatus()
                    }
                } catch {
                    self.message = "Bootstrap decode failed: \(error.localizedDescription)"
                }
            }
        }
    }

    private func applyIndexConfiguration(roots: [String], excludes: [String]) {
        if !roots.isEmpty {
            settings.rootsText = roots.joined(separator: "\n")
        }
        if !excludes.isEmpty {
            settings.excludesText = excludes.joined(separator: "\n")
        }
    }

    private func startInitialIndexIfNeeded(hasRecords: Bool, roots: [String]) -> Bool {
        guard !hasRecords, roots.isEmpty, !hasAttemptedInitialIndex, !isIndexing else { return false }
        hasAttemptedInitialIndex = true
        settings = IndexSettings.defaultSettings()
        message = "Building initial index..."
        rebuildIndex()
        return true
    }

    func refreshStatus() {
        // An explicit refresh consumes the deferred startup request as well.
        startupCountsGeneration = nil
        startupCountsDeadline = nil
        let generation = indexReadGeneration
        DispatchQueue.global(qos: .userInitiated).async {
            let output: CommandOutput
            do {
                output = try self.cli.run(["status", "--json"])
            } catch {
                DispatchQueue.main.async {
                    guard generation == self.indexReadGeneration else { return }
                    self.message = "Status failed: \(error.localizedDescription)"
                }
                return
            }

            DispatchQueue.main.async {
                guard generation == self.indexReadGeneration else { return }
                guard output.exitCode == 0 else {
                    self.message = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return
                }
                do {
                    let data = Data(output.stdout.utf8)
                    let status = try JSONDecoder().decode(IndexStatus.self, from: data)
                    self.status = status
                    guard !self.isIndexing else { return }
                    self.applyIndexConfiguration(roots: status.roots, excludes: status.excludes)
                    if self.startInitialIndexIfNeeded(hasRecords: status.total > 0, roots: status.roots) {
                        return
                    }
                    self.message = "Indexed \(Self.formatCount(status.total)) entries"
                    if status.total > 0 {
                        self.startSearchServiceIfNeeded()
                    }
                    if !status.roots.isEmpty {
                        self.startWatcherIfNeeded()
                    }
                } catch {
                    self.message = "Status decode failed: \(error.localizedDescription)"
                }
            }
        }
    }

    func refreshPermissionStatus() {
        DispatchQueue.global(qos: .utility).async {
            let status = Self.detectPermissionStatus()
            DispatchQueue.main.async {
                self.permissionStatus = status
            }
        }
    }

    func installGlobalHotKey() {
        guard hotKey == nil else { return }
        registerGlobalHotKey()
    }

    private func reinstallGlobalHotKey() {
        hotKey?.unregister()
        hotKey = nil
        registerGlobalHotKey()
    }

    private func registerGlobalHotKey() {
        let hotKey = GlobalHotKey(shortcut: globalHotKeyShortcut) { [weak self] in
            DispatchQueue.main.async {
                self?.showSearchWindow()
            }
        }
        hotKey.register()
        if hotKey.isRegistered {
            self.hotKey = hotKey
        } else {
            self.hotKey = nil
            message = "Global shortcut unavailable: \(globalHotKeyShortcut.title)"
        }
    }

    func showSearchWindow() {
        NSApp.activate(ignoringOtherApps: true)
        if let window = NSApp.windows.first(where: { $0.isVisible }) ?? NSApp.windows.first {
            window.makeKeyAndOrderFront(nil)
        }
    }

    func rebuildIndex() {
        let roots = settings.roots
        guard !roots.isEmpty else {
            message = "No index roots configured"
            return
        }
        // Reads begun before/during a rebuild must not replace its completed
        // configuration or trigger an initial index after isIndexing turns off.
        indexReadGeneration += 1
        startupCountsGeneration = nil
        startupCountsDeadline = nil
        isBootstrapping = false
        stopWatcher()
        stopSearchService()
        isIndexing = true
        message = "Indexing..."

        let excludes = settings.excludes
        DispatchQueue.global(qos: .userInitiated).async {
            var args = ["index", "--rebuild"]
            for exclude in excludes {
                args.append("--exclude")
                args.append(exclude)
            }
            args.append(contentsOf: roots)

            let output: CommandOutput
            do {
                output = try self.cli.run(args, timeout: 3600)
            } catch {
                DispatchQueue.main.async {
                    self.indexReadGeneration += 1
                    self.isIndexing = false
                    self.message = "Index failed: \(error.localizedDescription)"
                }
                return
            }

            DispatchQueue.main.async {
                self.indexReadGeneration += 1
                self.isIndexing = false
                if output.exitCode == 0 {
                    self.message = output.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
                    self.refreshStatus()
                    self.scheduleSearch()
                } else {
                    self.message = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                }
            }
        }
    }

    func openSelection() {
        guard let result = selectedResult else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: result.path))
    }

    func revealSelection() {
        guard let result = selectedResult else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: result.path)])
    }

    func copySelection() {
        guard let result = selectedResult else { return }
        copyText(result.path, message: "Copied path")
    }

    func copyNameSelection() {
        guard let result = selectedResult else { return }
        copyText(result.basename, message: "Copied name")
    }

    func copyParentSelection() {
        guard let result = selectedResult else { return }
        let parent = URL(fileURLWithPath: result.path).deletingLastPathComponent().path
        copyText(parent, message: "Copied parent folder")
    }

    func copyFileSelection() {
        guard let result = selectedResult else { return }
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        pasteboard.writeObjects([URL(fileURLWithPath: result.path) as NSURL])
        pasteboard.setString(result.path, forType: .string)
        message = "Copied file"
    }

    func quickLookSelection() {
        guard let result = selectedResult else { return }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/qlmanage")
        process.arguments = ["-p", result.path]
        try? process.run()
    }

    func addRoot(_ value: String) {
        settings.rootsText = IndexSettings.addLine(value, to: settings.rootsText)
    }

    func removeRoot(_ value: String) {
        settings.rootsText = IndexSettings.removeLine(value, from: settings.rootsText)
    }

    func chooseRootFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.canCreateDirectories = false
        panel.prompt = "Add"
        if panel.runModal() == .OK {
            for url in panel.urls {
                addRoot(url.path)
            }
        }
    }

    func addExclude(_ value: String) {
        settings.excludesText = IndexSettings.addLine(value, to: settings.excludesText)
    }

    func removeExclude(_ value: String) {
        settings.excludesText = IndexSettings.removeLine(value, from: settings.excludesText)
    }

    func resetDefaultExcludes() {
        settings.excludesText = IndexSettings.defaultExcludesText()
    }

    func openFullDiskAccessSettings() {
        let urls = [
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles"
        ]
        for value in urls {
            guard let url = URL(string: value) else { continue }
            if NSWorkspace.shared.open(url) {
                return
            }
        }
        NSWorkspace.shared.open(URL(fileURLWithPath: "/System/Applications/System Settings.app"))
    }

    func sort(by column: ResultSortColumn) {
        if sortColumn == column {
            sortAscending.toggle()
        } else {
            sortColumn = column
            sortAscending = column.defaultAscending
        }
        results = sortedResults(results)
    }

    func startSearchServiceIfNeeded() {
#if MACEVERY_TESTING
        childStartObserverForTesting?("serve")
#endif
        guard managesChildProcesses else {
#if MACEVERY_TESTING
            if startupCountsGeneration != nil && serviceStatusDeadline == nil {
                refreshSearchServiceStatus(retries: startupStatusRetryCount)
            }
#endif
            return
        }
        if let searchServiceProcess, searchServiceProcess.isRunning {
            if startupCountsGeneration != nil && serviceStatusDeadline == nil {
                refreshSearchServiceStatus(retries: startupStatusRetryCount)
            }
            return
        }

        let process = Process()
        process.executableURL = cli.executableURL
        process.arguments = ["serve", "--backend", searchPerformance.rawValue]
        process.environment = childProcessEnvironment()

        let stdout = Pipe()
        let stderr = Pipe()
        searchServiceStdout = stdout
        searchServiceStderr = stderr
        process.standardOutput = stdout
        process.standardError = stderr

        stdout.fileHandleForReading.readabilityHandler = { handle in
            _ = handle.availableData
        }
        stderr.fileHandleForReading.readabilityHandler = { handle in
            _ = handle.availableData
        }
        process.terminationHandler = { [weak self] process in
            DispatchQueue.main.async {
                if self?.searchServiceProcess === process {
                    self?.searchServiceProcess = nil
                    self?.cancelServiceStatusPolling()
                    self?.finishStartupCounts()
                }
            }
        }

        do {
            try process.run()
            searchServiceProcess = process
            refreshSearchServiceStatus(retries: startupStatusRetryCount)
        } catch {
            message = "Search service failed: \(error.localizedDescription)"
            finishStartupCounts()
        }
    }

    func restartSearchService() {
        stopSearchService()
        serviceStatus = nil
        if hasIndexedRecords {
            startSearchServiceIfNeeded()
            scheduleSearch()
        }
    }

    func refreshSearchServiceStatus(retries: Int = 0) {
        cancelServiceStatusPolling()
        let pollGeneration = serviceStatusGeneration
        let metadataGeneration = indexReadGeneration
        let process = searchServiceProcess
        // Repeated searches and mode restarts share the startup's fixed budget.
        let deadline = startupCountsDeadline ?? ProcessInfo.processInfo.systemUptime + startupCountsTimeout
        let remaining = max(0, deadline - ProcessInfo.processInfo.systemUptime)
        let timeout = DispatchWorkItem { [weak self, weak process] in
            guard let self, self.isCurrentStatusPoll(pollGeneration, metadataGeneration, process) else { return }
            self.cancelServiceStatusPolling()
            self.finishStartupCounts()
        }
        serviceStatusDeadline = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + remaining, execute: timeout)
        pollSearchServiceStatus(retries: retries, deadline: deadline, pollGeneration: pollGeneration,
                                metadataGeneration: metadataGeneration, process: process)
    }

    private func isCurrentStatusPoll(_ pollGeneration: Int, _ metadataGeneration: Int, _ process: Process?) -> Bool {
        pollGeneration == serviceStatusGeneration && metadataGeneration == indexReadGeneration
            && (!managesChildProcesses || (process != nil && searchServiceProcess === process))
    }

    private func pollSearchServiceStatus(retries: Int, deadline: TimeInterval, pollGeneration: Int,
                                         metadataGeneration: Int, process: Process?) {
        guard isCurrentStatusPoll(pollGeneration, metadataGeneration, process) else { return }
        let remaining = deadline - ProcessInfo.processInfo.systemUptime
        guard remaining > 0, let url = serviceStatusURL() else {
            cancelServiceStatusPolling()
            finishStartupCounts()
            return
        }
        var request = URLRequest(url: url)
        request.timeoutInterval = min(5, remaining)
        request.setValue("Bearer \(authToken)", forHTTPHeaderField: "Authorization")
        let task = searchSession.dataTask(with: request) { [weak self, weak process] data, response, error in
            let decoded = error == nil && (response as? HTTPURLResponse)?.statusCode == 200
                ? data.flatMap { try? JSONDecoder().decode(SearchServiceStatus.self, from: $0) } : nil
            DispatchQueue.main.async {
                guard let self, self.isCurrentStatusPoll(pollGeneration, metadataGeneration, process) else { return }
                self.serviceStatusTask = nil
                if let decoded {
                    self.serviceStatus = decoded
                    if decoded.ready == true && decoded.stale == false {
                        self.cancelServiceStatusPolling()
                        self.finishStartupCounts()
                        return
                    }
                }
                guard retries > 0, ProcessInfo.processInfo.systemUptime < deadline else {
                    self.cancelServiceStatusPolling()
                    self.finishStartupCounts()
                    return
                }
                let retry = DispatchWorkItem { [weak self, weak process] in
                    self?.pollSearchServiceStatus(retries: retries - 1, deadline: deadline,
                        pollGeneration: pollGeneration, metadataGeneration: metadataGeneration, process: process)
                }
                self.serviceStatusRetry = retry
                DispatchQueue.main.asyncAfter(deadline: .now() + self.serviceStatusRetryDelay, execute: retry)
            }
        }
        serviceStatusTask = task
        task.resume()
    }

    private func cancelServiceStatusPolling() {
        serviceStatusGeneration += 1
        serviceStatusTask?.cancel()
        serviceStatusTask = nil
        serviceStatusRetry?.cancel()
        serviceStatusRetry = nil
        serviceStatusDeadline?.cancel()
        serviceStatusDeadline = nil
    }

    private func finishStartupCounts() {
        guard let generation = startupCountsGeneration else { return }
        startupCountsGeneration = nil
        startupCountsDeadline = nil
        guard generation == indexReadGeneration, !isIndexing else { return }
        refreshStatus()
    }

    func stopSearchService() {
        cancelServiceStatusPolling()
        searchServiceStdout?.fileHandleForReading.readabilityHandler = nil
        searchServiceStderr?.fileHandleForReading.readabilityHandler = nil
        if let searchServiceProcess, searchServiceProcess.isRunning {
            searchServiceProcess.terminate()
        }
        searchServiceProcess = nil
        searchServiceStdout = nil
        searchServiceStderr = nil
    }

    func startWatcherIfNeeded() {
        guard !indexRoots.isEmpty else { return }
#if MACEVERY_TESTING
        childStartObserverForTesting?("watch")
#endif
        guard managesChildProcesses else { return }
        if let watchProcess, watchProcess.isRunning { return }

        let process = Process()
        process.executableURL = cli.executableURL
        process.arguments = ["watch"]
        process.environment = childProcessEnvironment()

        let stdout = Pipe()
        let stderr = Pipe()
        watchStdout = stdout
        watchStderr = stderr
        process.standardOutput = stdout
        process.standardError = stderr

        stdout.fileHandleForReading.readabilityHandler = { handle in
            _ = handle.availableData
        }
        stderr.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            DispatchQueue.main.async {
                self?.message = text.trimmingCharacters(in: .whitespacesAndNewlines)
            }
        }
        process.terminationHandler = { [weak self] process in
            DispatchQueue.main.async {
                if self?.watchProcess === process {
                    self?.watchProcess = nil
                }
            }
        }

        do {
            try process.run()
            watchProcess = process
            if !message.contains("Watcher active") {
                message = "\(message) • Watcher active"
            }
        } catch {
            message = "Watcher failed: \(error.localizedDescription)"
        }
    }

    func stopWatcher() {
        watchStdout?.fileHandleForReading.readabilityHandler = nil
        watchStderr?.fileHandleForReading.readabilityHandler = nil
        if let watchProcess, watchProcess.isRunning {
            watchProcess.terminate()
        }
        watchProcess = nil
        watchStdout = nil
        watchStderr = nil
    }

    func select(_ result: SearchResult) {
        selectedPath = result.path
    }

    private func copyText(_ value: String, message: String) {
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        pasteboard.setString(value, forType: .string)
        self.message = message
    }

    deinit {
        pendingSearch?.cancel()
        activeSearchTask?.cancel()
        activeCLICancellation?.cancel()
        cliSearchQueue.cancelAllOperations()
        hotKey?.unregister()
        stopSearchService()
        stopWatcher()
    }

    private func scheduleSearch() {
        // Invalidate immediately, including clear and the debounce interval.
        searchGeneration += 1
        let generation = searchGeneration
        pendingSearch?.cancel()
        activeSearchTask?.cancel()
        activeSearchTask = nil
        activeCLICancellation?.cancel()
        activeCLICancellation = nil
        cliSearchQueue.cancelAllOperations()
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            results = []
            selectedPath = nil
            isSearching = false
            return
        }

        let item = DispatchWorkItem { [weak self] in
            guard let self, generation == self.searchGeneration else { return }
            self.performSearch(trimmed, generation: generation)
        }
        pendingSearch = item
        DispatchQueue.main.asyncAfter(deadline: .now() + searchDebounceDelay, execute: item)
    }

    private func performSearch(_ query: String, generation: Int) {
        guard generation == searchGeneration else { return }
        isSearching = true
        startSearchServiceIfNeeded()
        performServiceSearch(query, generation: generation)
    }

    private func performServiceSearch(_ query: String, generation: Int, busyAttempt: Int = 0) {
        guard let url = serviceSearchURL(for: query) else {
            performCLISearch(query, generation: generation)
            return
        }

        var request = URLRequest(url: url)
        request.timeoutInterval = 15
        request.setValue("Bearer \(authToken)", forHTTPHeaderField: "Authorization")
        let task = searchSession.dataTask(with: request) { [weak self] data, response, error in
            guard let self else { return }
            let decoded: [SearchResult]?
            if error == nil, (response as? HTTPURLResponse)?.statusCode == 200, let data {
                decoded = try? JSONDecoder().decode([SearchResult].self, from: data)
            } else {
                decoded = nil
            }
            DispatchQueue.main.async {
                guard generation == self.searchGeneration else { return }
                self.activeSearchTask = nil
                // Cancellation is intentional and must never launch a CLI fallback.
                if (error as? URLError)?.code == .cancelled {
                    self.isSearching = false
                    return
                }
                let statusCode = (response as? HTTPURLResponse)?.statusCode
                if statusCode == 429 || statusCode == 503 {
                    // The service is applying backpressure. A second CLI search
                    // would bypass that CPU limit, so retry only the latest query.
                    if busyAttempt < Self.serviceBusyRetryDelays.count {
                        let delay = Self.serviceBusyRetryDelays[busyAttempt]
                        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                            guard generation == self.searchGeneration else { return }
                            self.performServiceSearch(query, generation: generation, busyAttempt: busyAttempt + 1)
                        }
                    } else {
                        self.isSearching = false
                        self.message = "Search service busy; try again"
                    }
                    return
                }
                guard let decoded else {
                    self.performCLISearch(query, generation: generation)
                    return
                }
                self.isSearching = false
                let sorted = self.sortedResults(decoded)
                self.results = sorted
                self.selectedPath = sorted.first?.path
                let backend = self.serviceStatus?.backendLabel ?? "service"
                self.message = "\(decoded.count) results • \(backend)"
            }
        }
        activeSearchTask = task
        task.resume()
    }

    private func performCLISearch(_ query: String, generation: Int) {
        guard generation == searchGeneration else { return }
        let cancellation = CommandCancellation()
        activeCLICancellation = cancellation
        // Only one child can run; cancelled queued operations never launch.
        cliSearchQueue.addOperation { [weak self] in
            guard let self, !cancellation.isCancelled else { return }
            let output: CommandOutput
            do {
                output = try self.cli.run(
                    ["search", query, "--limit", "300", "--json"],
                    cancellation: cancellation,
                    timeout: 15
                )
            } catch {
                DispatchQueue.main.async {
                    guard generation == self.searchGeneration, !cancellation.isCancelled else { return }
                    self.activeCLICancellation = nil
                    self.isSearching = false
                    self.message = "Search failed: \(error.localizedDescription)"
                }
                return
            }
            guard !cancellation.isCancelled else { return }
            let decoded = output.exitCode == 0
                ? try? JSONDecoder().decode([SearchResult].self, from: Data(output.stdout.utf8))
                : nil
            DispatchQueue.main.async {
                guard generation == self.searchGeneration, !cancellation.isCancelled else { return }
                self.activeCLICancellation = nil
                self.isSearching = false
                guard output.exitCode == 0 else {
                    self.message = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return
                }
                guard let decoded else {
                    self.message = "Search decode failed"
                    return
                }
                let sorted = self.sortedResults(decoded)
                self.results = sorted
                self.selectedPath = sorted.first?.path
                self.message = "\(decoded.count) results"
            }
        }
    }

    private func serviceSearchURL(for query: String) -> URL? {
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = 17649
        components.path = "/search"
        components.queryItems = [
            URLQueryItem(name: "q", value: query),
            URLQueryItem(name: "limit", value: "300")
        ]
        return components.url
    }

    private func serviceStatusURL() -> URL? {
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = 17649
        components.path = "/status"
        return components.url
    }

    private func childProcessEnvironment() -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        environment["MACEVERY_EXIT_WITH_PARENT"] = "1"
        environment["MACEVERY_AUTH_TOKEN"] = authToken
        return environment
    }

    private func sortedResults(_ values: [SearchResult]) -> [SearchResult] {
        values.sorted { lhs, rhs in
            let primary = compare(lhs, rhs, by: sortColumn, ascending: sortAscending)
            if primary != 0 {
                return primary < 0
            }
            let score = compareInt(lhs.score, rhs.score, ascending: true)
            if score != 0 {
                return score < 0
            }
            return lhs.path.localizedCaseInsensitiveCompare(rhs.path) == .orderedAscending
        }
    }

    private func compare(_ lhs: SearchResult, _ rhs: SearchResult, by column: ResultSortColumn, ascending: Bool) -> Int {
        switch column {
        case .relevance:
            return compareInt(lhs.score, rhs.score, ascending: ascending)
        case .name:
            return compareText(lhs.basename, rhs.basename, ascending: ascending)
        case .path:
            return compareText(lhs.path, rhs.path, ascending: ascending)
        case .kind:
            return compareText(lhs.kind, rhs.kind, ascending: ascending)
        case .size:
            return compareOptionalInt(lhs.size, rhs.size, ascending: ascending)
        case .modified:
            return compareOptionalInt(lhs.mtime, rhs.mtime, ascending: ascending)
        }
    }

    private func compareText(_ lhs: String, _ rhs: String, ascending: Bool) -> Int {
        let result = lhs.localizedCaseInsensitiveCompare(rhs)
        let value: Int
        switch result {
        case .orderedAscending: value = -1
        case .orderedDescending: value = 1
        case .orderedSame: value = 0
        }
        return ascending ? value : -value
    }

    private func compareInt(_ lhs: Int64, _ rhs: Int64, ascending: Bool) -> Int {
        let value: Int
        if lhs < rhs {
            value = -1
        } else if lhs > rhs {
            value = 1
        } else {
            value = 0
        }
        return ascending ? value : -value
    }

    private func compareOptionalInt(_ lhs: Int64?, _ rhs: Int64?, ascending: Bool) -> Int {
        switch (lhs, rhs) {
        case (nil, nil):
            return 0
        case (nil, _):
            return 1
        case (_, nil):
            return -1
        case let (lhs?, rhs?):
            return compareInt(lhs, rhs, ascending: ascending)
        }
    }

    private static func detectPermissionStatus() -> PermissionStatus {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let probes = [
            home.appendingPathComponent("Library/Mail"),
            home.appendingPathComponent("Library/Messages"),
            home.appendingPathComponent("Pictures/Photos Library.photoslibrary")
        ]
        let existing = probes.filter { FileManager.default.fileExists(atPath: $0.path) }
        guard !existing.isEmpty else {
            return .unknown
        }

        let accessible = existing.filter { url in
            (try? FileManager.default.contentsOfDirectory(atPath: url.path)) != nil
        }
        return accessible.count == existing.count ? .likelyGranted : .limited
    }

    static func formatCount(_ value: Int64) -> String {
        let formatter = NumberFormatter()
        formatter.numberStyle = .decimal
        return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
    }

    static func formatOptionalCount(_ value: Int64?) -> String {
        value.map(formatCount) ?? "—"
    }
}

struct IndexSettings {
    var rootsText: String
    var excludesText: String

    var roots: [String] {
        lines(from: rootsText)
    }

    var excludes: [String] {
        lines(from: excludesText)
    }

    static func defaultSettings() -> IndexSettings {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return IndexSettings(
            rootsText: [
                home,
                "/Applications"
            ].joined(separator: "\n"),
            excludesText: defaultExcludesText()
        )
    }

    static func defaultExcludesText() -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        var excludes = [
            "node_modules",
            ".git",
            "target",
            "dist",
            "build",
            ".build",
            ".Trash",
            ".codex",
            ".gemini",
            ".cursor",
            ".local/share/macevery",
            "Library/Caches",
            "Library/Developer",
            "Library/Application Support",
            "Library/Containers",
            "Library/Group Containers",
            "Library/Mail",
            "Library/Messages",
            "Library/Logs",
            "Library/Saved Application State",
            ".cache",
            ".npm",
            ".cargo/registry",
            "DerivedData",
            ".DS_Store"
        ]
        excludes.append(contentsOf: [
            "\(home)/Library/Safari",
            "\(home)/Library/Cookies",
            "\(home)/Library/Metadata",
            "\(home)/Library/Accounts"
        ])
        return excludes.joined(separator: "\n")
    }

    static func addLine(_ value: String, to text: String) -> String {
        let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return text }
        var values = lines(from: text)
        if !values.contains(value) {
            values.append(value)
        }
        return values.joined(separator: "\n")
    }

    static func removeLine(_ value: String, from text: String) -> String {
        lines(from: text)
            .filter { $0 != value }
            .joined(separator: "\n")
    }

    private static func lines(from text: String) -> [String] {
        text
            .split(whereSeparator: \.isNewline)
            .map { String($0).trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }

    private func lines(from text: String) -> [String] {
        Self.lines(from: text)
    }
}

#if !MACEVERY_TESTING
@main
struct MacEveryDesktopApp: App {
    @StateObject private var model = SearchViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .frame(minWidth: 980, minHeight: 620)
                .onAppear {
                    model.installGlobalHotKey()
                    model.refreshPermissionStatus()
                    model.bootstrapIfNeeded()
                }
        }
        .commands {
            CommandMenu("Result") {
                Button("Open") { model.openSelection() }
                    .keyboardShortcut(.return, modifiers: [])
                    .disabled(model.selectedResult == nil)
                Button("Reveal in Finder") { model.revealSelection() }
                    .keyboardShortcut(.return, modifiers: [.command])
                    .disabled(model.selectedResult == nil)
                Button("Quick Look") { model.quickLookSelection() }
                    .keyboardShortcut(" ", modifiers: [])
                    .disabled(model.selectedResult == nil)
                Divider()
                Button("Copy Path") { model.copySelection() }
                    .keyboardShortcut("c", modifiers: [.command])
                    .disabled(model.selectedResult == nil)
                Button("Copy Name") { model.copyNameSelection() }
                    .keyboardShortcut("c", modifiers: [.command, .shift])
                    .disabled(model.selectedResult == nil)
                Button("Copy Parent Folder") { model.copyParentSelection() }
                    .disabled(model.selectedResult == nil)
            }
        }
    }
}

#endif

struct ContentView: View {
    @EnvironmentObject private var model: SearchViewModel
    @State private var showingSettings = false
    @State private var showingOnboarding = false
    @State private var columnWidths = ResultColumnWidths.load()

    var body: some View {
        VStack(spacing: 0) {
            topBar
            Divider()
            HStack(spacing: 0) {
                sidebar
                Divider()
                resultsPane
            }
            Divider()
            statusBar
        }
        .sheet(isPresented: $showingSettings) {
            SettingsView()
                .environmentObject(model)
                .frame(width: 720, height: 700)
        }
        .sheet(isPresented: $showingOnboarding) {
            OnboardingView(
                onOpenFullDiskAccess: {
                    model.openFullDiskAccessSettings()
                },
                onOpenSettings: {
                    completeOnboarding()
                    showingSettings = true
                },
                onRebuild: {
                    completeOnboarding()
                    model.rebuildIndex()
                },
                onDone: {
                    completeOnboarding()
                }
            )
            .environmentObject(model)
            .frame(width: 560, height: 430)
        }
        .onAppear {
            if !UserDefaults.standard.bool(forKey: "hasCompletedOnboarding") {
                showingOnboarding = true
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            model.refreshPermissionStatus()
        }
    }

    private var topBar: some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
            TextField("Search files and paths", text: $model.query)
                .textFieldStyle(.plain)
                .font(.system(size: 22, weight: .medium, design: .rounded))
                .submitLabel(.search)
                .onSubmit {
                    model.openSelection()
                }
            if !model.query.isEmpty {
                Button {
                    model.query = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .help("Clear")
            }
            if model.isSearching {
                ProgressView()
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
        .background(Color(nsColor: .textBackgroundColor))
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 8) {
                Label("Index", systemImage: "externaldrive.connected.to.line.below")
                    .font(.headline)
                MetricLine(label: "Entries", value: SearchViewModel.formatOptionalCount(model.status?.total))
                MetricLine(label: "Files", value: SearchViewModel.formatOptionalCount(model.status?.files))
                MetricLine(label: "Folders", value: SearchViewModel.formatOptionalCount(model.status?.dirs))
                MetricLine(label: "Apps", value: SearchViewModel.formatOptionalCount(model.status?.apps))
            }

            VStack(alignment: .leading, spacing: 8) {
                Label("Actions", systemImage: "bolt")
                    .font(.headline)
                Button {
                    model.openSelection()
                } label: {
                    Label("Open", systemImage: "arrow.up.forward.app")
                }
                .disabled(model.selectedResult == nil)
                Button {
                    model.revealSelection()
                } label: {
                    Label("Reveal", systemImage: "finder")
                }
                .disabled(model.selectedResult == nil)
                Button {
                    model.copySelection()
                } label: {
                    Label("Copy Path", systemImage: "doc.on.doc")
                }
                .disabled(model.selectedResult == nil)
            }

            VStack(alignment: .leading, spacing: 8) {
                Label("Search", systemImage: "speedometer")
                    .font(.headline)
                MetricLine(label: "Mode", value: model.serviceStatus?.backendLabel ?? model.searchPerformance.title)
                if let serviceStatus = model.serviceStatus {
                    MetricLine(label: serviceStatus.actualIndexBytes == nil ? "Estimate" : "Index", value: serviceStatus.memoryLabel)
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Label("Permissions", systemImage: "lock.shield")
                    .font(.headline)
                Label(model.permissionStatus.label, systemImage: model.permissionStatus.iconName)
                    .foregroundStyle(model.permissionStatus.color)
                Text("Full Disk Access helps index protected folders.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 10) {
                    Button {
                        model.openFullDiskAccessSettings()
                    } label: {
                        Label("Open", systemImage: "gear")
                    }
                    Button {
                        model.refreshPermissionStatus()
                    } label: {
                        Label("Check", systemImage: "checkmark.circle")
                    }
                }
            }

            Spacer()

            VStack(alignment: .leading, spacing: 8) {
                Button {
                    showingSettings = true
                } label: {
                    Label("Settings", systemImage: "slider.horizontal.3")
                }
                Button {
                    model.rebuildIndex()
                } label: {
                    Label("Rebuild Index", systemImage: "arrow.clockwise")
                }
                .disabled(model.isIndexing)
                Button {
                    model.refreshStatus()
                } label: {
                    Label("Refresh", systemImage: "goforward")
                }
            }
        }
        .buttonStyle(.borderless)
        .padding(16)
        .frame(width: 220)
        .background(Color(nsColor: .controlBackgroundColor))
    }

    private var resultsPane: some View {
        GeometryReader { proxy in
            let tableWidth = max(proxy.size.width, columnWidths.contentWidth)

            ScrollView(.horizontal) {
                VStack(spacing: 0) {
                    ResultHeader(
                        widths: columnWidths,
                        tableWidth: tableWidth,
                        sortColumn: model.sortColumn,
                        sortAscending: model.sortAscending,
                        onSort: model.sort,
                        onResize: resizeColumn
                    )
                    if model.results.isEmpty {
                        EmptyResultsView(
                            hasQuery: !model.query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                            hasIndex: model.hasIndexedRecords,
                            isIndexing: model.isIndexing,
                            onIndex: { model.rebuildIndex() }
                        )
                        .frame(width: tableWidth)
                        .frame(maxHeight: .infinity)
                    } else {
                        ScrollView {
                            LazyVStack(spacing: 0) {
                                ForEach(model.results) { result in
                                    ResultRow(
                                        result: result,
                                        isSelected: result.path == model.selectedPath,
                                        widths: columnWidths,
                                        tableWidth: tableWidth
                                    )
                                    .onTapGesture {
                                        model.select(result)
                                    }
                                    .onTapGesture(count: 2) {
                                        model.select(result)
                                        model.openSelection()
                                    }
                                    .onDrag {
                                        NSItemProvider(object: URL(fileURLWithPath: result.path) as NSURL)
                                    }
                                    .contextMenu {
                                        Button {
                                            model.select(result)
                                            model.openSelection()
                                        } label: {
                                            Label("Open", systemImage: "arrow.up.forward.app")
                                        }
                                        Button {
                                            model.select(result)
                                            model.revealSelection()
                                        } label: {
                                            Label("Reveal in Finder", systemImage: "finder")
                                        }
                                        Button {
                                            model.select(result)
                                            model.quickLookSelection()
                                        } label: {
                                            Label("Quick Look", systemImage: "eye")
                                        }
                                        Divider()
                                        Button {
                                            model.select(result)
                                            model.copySelection()
                                        } label: {
                                            Label("Copy Path", systemImage: "doc.on.doc")
                                        }
                                        Button {
                                            model.select(result)
                                            model.copyNameSelection()
                                        } label: {
                                            Label("Copy Name", systemImage: "textformat")
                                        }
                                        Button {
                                            model.select(result)
                                            model.copyParentSelection()
                                        } label: {
                                            Label("Copy Parent Folder", systemImage: "folder")
                                        }
                                        Button {
                                            model.select(result)
                                            model.copyFileSelection()
                                        } label: {
                                            Label("Copy File", systemImage: "doc")
                                        }
                                    }
                                }
                            }
                            .frame(width: tableWidth, alignment: .topLeading)
                        }
                    }
                }
                .frame(width: tableWidth, height: proxy.size.height, alignment: .topLeading)
            }
            .frame(width: proxy.size.width, height: proxy.size.height)
        }
        .background(Color(nsColor: .textBackgroundColor))
    }

    private func resizeColumn(_ column: ResultSortColumn, to width: CGFloat) {
        columnWidths.setWidth(width, for: column)
        columnWidths.save()
    }

    private var statusBar: some View {
        HStack(spacing: 12) {
            if model.isIndexing {
                ProgressView()
                    .controlSize(.small)
                Text("Indexing")
                    .fontWeight(.medium)
            }
            Text(model.message)
                .lineLimit(1)
                .foregroundStyle(.secondary)
            Spacer()
            if let path = model.selectedPath {
                Text(path)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .foregroundStyle(.secondary)
            }
        }
        .font(.system(size: 12))
        .padding(.horizontal, 14)
        .padding(.vertical, 7)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private func completeOnboarding() {
        UserDefaults.standard.set(true, forKey: "hasCompletedOnboarding")
        showingOnboarding = false
    }
}

struct MetricLine: View {
    let label: String
    let value: String

    var body: some View {
        HStack {
            Text(label)
                .foregroundStyle(.secondary)
            Spacer()
            Text(value)
                .fontDesign(.monospaced)
        }
        .font(.system(size: 12))
    }
}

struct ResultHeader: View {
    let widths: ResultColumnWidths
    let tableWidth: CGFloat
    let sortColumn: ResultSortColumn
    let sortAscending: Bool
    let onSort: (ResultSortColumn) -> Void
    let onResize: (ResultSortColumn, CGFloat) -> Void

    var body: some View {
        HStack(spacing: 12) {
            HeaderCell(
                column: .name,
                width: widths.name,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort,
                onResize: onResize
            )
            HeaderCell(
                column: .path,
                width: widths.path,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort,
                onResize: onResize
            )
            HeaderCell(
                column: .kind,
                width: widths.kind,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort,
                onResize: onResize
            )
            HeaderCell(
                column: .size,
                width: widths.size,
                alignment: .trailing,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort,
                onResize: onResize
            )
            HeaderCell(
                column: .modified,
                width: widths.modified,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort,
                onResize: onResize
            )
            Spacer(minLength: 0)
        }
        .font(.system(size: 11, weight: .semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .frame(width: tableWidth, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor))
    }
}

struct HeaderCell: View {
    let column: ResultSortColumn
    let width: CGFloat
    let alignment: Alignment
    let sortColumn: ResultSortColumn
    let sortAscending: Bool
    let onSort: (ResultSortColumn) -> Void
    let onResize: (ResultSortColumn, CGFloat) -> Void
    @State private var dragStartWidth: CGFloat?

    var body: some View {
        Button {
            onSort(column)
        } label: {
            HStack(spacing: 4) {
                Text(column.title)
                    .lineLimit(1)
                if sortColumn == column {
                    Image(systemName: sortAscending ? "chevron.up" : "chevron.down")
                        .font(.system(size: 9, weight: .bold))
                }
            }
            .frame(width: width, alignment: alignment)
        }
        .buttonStyle(.plain)
        .help("Sort by \(column.title)")
        .overlay(alignment: .trailing) {
            ColumnResizeHandle(columnTitle: column.title) { translation in
                if dragStartWidth == nil {
                    dragStartWidth = width
                }
                onResize(column, (dragStartWidth ?? width) + translation)
            } onEnded: {
                dragStartWidth = nil
            }
        }
    }
}

struct ColumnResizeHandle: View {
    let columnTitle: String
    let onChanged: (CGFloat) -> Void
    let onEnded: () -> Void
    @State private var isHovering = false

    var body: some View {
        ZStack {
            Rectangle()
                .fill(Color.clear)
                .frame(width: 10)
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(isHovering ? 0.9 : 0.45))
                .frame(width: 1)
        }
        .contentShape(Rectangle())
        .onHover { hovering in
            isHovering = hovering
        }
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { value in
                    onChanged(value.translation.width)
                }
                .onEnded { _ in
                    onEnded()
                }
        )
        .help("Resize \(columnTitle)")
    }
}

struct ResultRow: View {
    let result: SearchResult
    let isSelected: Bool
    let widths: ResultColumnWidths
    let tableWidth: CGFloat

    var body: some View {
        HStack(spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: iconName)
                    .foregroundStyle(iconColor)
                    .frame(width: 18)
                Text(result.basename)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .frame(width: widths.name, alignment: .leading)

            Text(result.path)
                .lineLimit(1)
                .truncationMode(.middle)
                .foregroundStyle(.secondary)
                .frame(width: widths.path, alignment: .leading)

            Text(result.kind)
                .foregroundStyle(.secondary)
                .frame(width: widths.kind, alignment: .leading)
            Text(formatSize(result.size))
                .foregroundStyle(.secondary)
                .fontDesign(.monospaced)
                .frame(width: widths.size, alignment: .trailing)
            Text(formatDate(result.mtime))
                .foregroundStyle(.secondary)
                .frame(width: widths.modified, alignment: .leading)
            Spacer(minLength: 0)
        }
        .font(.system(size: 13))
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .frame(width: tableWidth, alignment: .leading)
        .background(isSelected ? Color.accentColor.opacity(0.18) : Color.clear)
        .contentShape(Rectangle())
        .help(result.path)
    }

    private var iconName: String {
        switch result.kind {
        case "dir": return "folder"
        case "app": return "app"
        case "symlink": return "link"
        default: return "doc"
        }
    }

    private var iconColor: Color {
        switch result.kind {
        case "dir": return .blue
        case "app": return .purple
        case "symlink": return .orange
        default: return .secondary
        }
    }

    private func formatSize(_ value: Int64?) -> String {
        guard let value else { return "-" }
        return ByteCountFormatter.string(fromByteCount: value, countStyle: .file)
    }

    private func formatDate(_ value: Int64?) -> String {
        guard let value else { return "-" }
        let date = Date(timeIntervalSince1970: TimeInterval(value))
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .short
        return formatter.string(from: date)
    }
}

struct EmptyResultsView: View {
    let hasQuery: Bool
    let hasIndex: Bool
    let isIndexing: Bool
    let onIndex: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: iconName)
                .font(.system(size: 34))
                .foregroundStyle(.secondary)
            Text(title)
                .font(.headline)
                .foregroundStyle(.secondary)
            if !hasIndex && !isIndexing {
                Button {
                    onIndex()
                } label: {
                    Label("Build Index", systemImage: "arrow.clockwise")
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var iconName: String {
        if isIndexing { return "externaldrive.badge.timemachine" }
        if !hasIndex { return "externaldrive.badge.plus" }
        return hasQuery ? "magnifyingglass" : "tray"
    }

    private var title: String {
        if isIndexing { return "Indexing" }
        if !hasIndex { return "No Index Yet" }
        return hasQuery ? "No Results" : "No Query"
    }
}

struct OnboardingView: View {
    @EnvironmentObject private var model: SearchViewModel
    let onOpenFullDiskAccess: () -> Void
    let onOpenSettings: () -> Void
    let onRebuild: () -> Void
    let onDone: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text("MacEvery")
                        .font(.title2)
                        .fontWeight(.semibold)
                    Text("Local filename and path search")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button {
                    onDone()
                } label: {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.borderless)
                .help("Close")
            }

            VStack(alignment: .leading, spacing: 12) {
                OnboardingRow(
                    title: "Full Disk Access",
                    value: model.permissionStatus.label,
                    systemImage: model.permissionStatus.iconName,
                    color: model.permissionStatus.color
                ) {
                    Button {
                        onOpenFullDiskAccess()
                    } label: {
                        Label("Open", systemImage: "gear")
                    }
                }

                if model.permissionStatus == .unknown || model.permissionStatus == .limited {
                    HStack(spacing: 6) {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                        Text("Full Disk Access not granted — index may be incomplete.")
                            .font(.system(size: 11))
                            .foregroundStyle(.orange)
                    }
                    .padding(.leading, 34)
                }

                OnboardingRow(
                    title: "Index Roots",
                    value: "\(model.settings.roots.count) folders",
                    systemImage: "folder",
                    color: .blue
                ) {
                    Button {
                        onOpenSettings()
                    } label: {
                        Label("Edit", systemImage: "slider.horizontal.3")
                    }
                }

                OnboardingRow(
                    title: "Index",
                    value: SearchViewModel.formatOptionalCount(model.status?.total),
                    systemImage: "externaldrive.connected.to.line.below",
                    color: .secondary
                ) {
                    Button {
                        onRebuild()
                    } label: {
                        Label("Rebuild", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.isIndexing)
                }
            }

            Spacer()

            HStack {
                Button {
                    onOpenSettings()
                } label: {
                    Label("Settings", systemImage: "slider.horizontal.3")
                }
                Spacer()
                Button("Skip") {
                    onDone()
                }
                Button {
                    onRebuild()
                } label: {
                    Label("Build Index", systemImage: "arrow.clockwise")
                }
                .keyboardShortcut(.defaultAction)
                .disabled(model.isIndexing || model.settings.roots.isEmpty)
            }
        }
        .padding(22)
    }
}

struct OnboardingRow<Accessory: View>: View {
    let title: String
    let value: String
    let systemImage: String
    let color: Color
    @ViewBuilder let accessory: () -> Accessory

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: systemImage)
                .foregroundStyle(color)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .fontWeight(.medium)
                Text(value)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            accessory()
        }
        .padding(.vertical, 8)
    }
}

struct SettingsView: View {
    @EnvironmentObject private var model: SearchViewModel
    @Environment(\.dismiss) private var dismiss
    @State private var rootDraft = ""
    @State private var excludeDraft = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("Settings")
                    .font(.title2)
                    .fontWeight(.semibold)
                Spacer()
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.borderless)
            }

            VStack(alignment: .leading, spacing: 8) {
                Label("Search Performance", systemImage: "speedometer")
                    .font(.headline)
                Picker("Search Performance", selection: $model.searchPerformance) {
                    ForEach(SearchPerformanceMode.allCases) { mode in
                        Text(mode.title).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                Text(model.searchPerformance.detail)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let buildLabel = Bundle.main.object(forInfoDictionaryKey: "MacEveryBuildLabel") as? String {
                    Text("Build \(buildLabel)")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Label("Global Shortcut", systemImage: "keyboard")
                    .font(.headline)
                Picker("Global Shortcut", selection: $model.globalHotKeyShortcut) {
                    ForEach(GlobalHotKeyShortcut.allCases) { shortcut in
                        Text(shortcut.title).tag(shortcut)
                    }
                }
                .pickerStyle(.segmented)
                Text(model.globalHotKeyShortcut.detail)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            SettingsListEditor(
                title: "Index Roots",
                systemImage: "folder.badge.gearshape",
                values: model.settings.roots,
                draft: $rootDraft,
                placeholder: "/Users/name",
                onAdd: { value in model.addRoot(value) },
                onRemove: { value in model.removeRoot(value) },
                onPick: { model.chooseRootFolder() }
            )
            .frame(minHeight: 205)

            SettingsListEditor(
                title: "Excludes",
                systemImage: "line.3.horizontal.decrease.circle",
                values: model.settings.excludes,
                draft: $excludeDraft,
                placeholder: "Library/Caches",
                onAdd: { value in model.addExclude(value) },
                onRemove: { value in model.removeExclude(value) },
                onPick: nil,
                trailingAction: {
                    model.resetDefaultExcludes()
                }
            )
            .frame(minHeight: 175)

            HStack {
                Spacer()
                Button("Cancel") {
                    dismiss()
                }
                Button {
                    dismiss()
                    model.rebuildIndex()
                } label: {
                    Label("Rebuild", systemImage: "arrow.clockwise")
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
    }
}

struct SettingsListEditor: View {
    let title: String
    let systemImage: String
    let values: [String]
    @Binding var draft: String
    let placeholder: String
    let onAdd: (String) -> Void
    let onRemove: (String) -> Void
    let onPick: (() -> Void)?
    var trailingAction: (() -> Void)? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Label(title, systemImage: systemImage)
                    .font(.headline)
                Spacer()
                if let trailingAction {
                    Button {
                        trailingAction()
                    } label: {
                        Image(systemName: "arrow.counterclockwise")
                    }
                    .buttonStyle(.borderless)
                    .help("Reset")
                }
            }

            HStack(spacing: 8) {
                TextField(placeholder, text: $draft)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(.body, design: .monospaced))
                    .onSubmit(addDraft)
                Button {
                    addDraft()
                } label: {
                    Image(systemName: "plus")
                }
                .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .help("Add")
                if let onPick {
                    Button {
                        onPick()
                    } label: {
                        Image(systemName: "folder.badge.plus")
                    }
                    .help("Choose Folder")
                }
            }

            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(values, id: \.self) { value in
                        HStack(spacing: 8) {
                            Text(value)
                                .font(.system(.body, design: .monospaced))
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Spacer()
                            Button {
                                onRemove(value)
                            } label: {
                                Image(systemName: "minus.circle")
                            }
                            .buttonStyle(.borderless)
                            .help("Remove")
                        }
                        .padding(.horizontal, 8)
                        .padding(.vertical, 5)
                        Divider()
                    }
                }
            }
            .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.25)))
        }
    }

    private func addDraft() {
        let value = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return }
        onAdd(value)
        draft = ""
    }
}
