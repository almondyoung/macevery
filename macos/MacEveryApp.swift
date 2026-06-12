import AppKit
import Carbon
import Foundation
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

struct CommandOutput {
    let stdout: String
    let stderr: String
    let exitCode: Int32
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

final class GlobalHotKey {
    private var hotKeyRef: EventHotKeyRef?
    private var handlerRef: EventHandlerRef?
    private let callback: () -> Void

    init(callback: @escaping () -> Void) {
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
            UInt32(kVK_Space),
            UInt32(controlKey),
            hotKeyID,
            GetApplicationEventTarget(),
            0,
            &hotKeyRef
        )
        if registerStatus != noErr {
            unregister()
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
    }

    deinit {
        unregister()
    }
}

final class MacEveryCLI {
    let executableURL: URL

    init() {
        executableURL = MacEveryCLI.findExecutable()
    }

    func run(_ arguments: [String]) throws -> CommandOutput {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = arguments

        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr

        try process.run()
        process.waitUntilExit()

        let outData = stdout.fileHandleForReading.readDataToEndOfFile()
        let errData = stderr.fileHandleForReading.readDataToEndOfFile()
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
    @Published var isSearching = false
    @Published var isIndexing = false
    @Published var message = "Ready"
    @Published var settings = IndexSettings.defaultSettings()
    @Published var permissionStatus: PermissionStatus = .unknown
    @Published var sortColumn: ResultSortColumn = .relevance
    @Published var sortAscending = ResultSortColumn.relevance.defaultAscending

    private let cli = MacEveryCLI()
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

    var selectedResult: SearchResult? {
        guard let selectedPath else { return nil }
        return results.first { $0.path == selectedPath }
    }

    func refreshStatus() {
        DispatchQueue.global(qos: .userInitiated).async {
            let output: CommandOutput
            do {
                output = try self.cli.run(["status", "--json"])
            } catch {
                DispatchQueue.main.async {
                    self.message = "Status failed: \(error.localizedDescription)"
                }
                return
            }

            DispatchQueue.main.async {
                guard output.exitCode == 0 else {
                    self.message = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return
                }
                do {
                    let data = Data(output.stdout.utf8)
                    let status = try JSONDecoder().decode(IndexStatus.self, from: data)
                    self.status = status
                    if !status.roots.isEmpty {
                        self.settings.rootsText = status.roots.joined(separator: "\n")
                    }
                    if !status.excludes.isEmpty {
                        self.settings.excludesText = status.excludes.joined(separator: "\n")
                    }
                    if status.total == 0 && status.roots.isEmpty && !self.hasAttemptedInitialIndex {
                        self.hasAttemptedInitialIndex = true
                        self.settings = IndexSettings.defaultSettings()
                        self.message = "Building initial index..."
                        self.rebuildIndex()
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
        let hotKey = GlobalHotKey { [weak self] in
            DispatchQueue.main.async {
                self?.showSearchWindow()
            }
        }
        hotKey.register()
        self.hotKey = hotKey
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
                output = try self.cli.run(args)
            } catch {
                DispatchQueue.main.async {
                    self.isIndexing = false
                    self.message = "Index failed: \(error.localizedDescription)"
                }
                return
            }

            DispatchQueue.main.async {
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
        if let searchServiceProcess, searchServiceProcess.isRunning { return }

        let process = Process()
        process.executableURL = cli.executableURL
        process.arguments = ["serve"]

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
                }
            }
        }

        do {
            try process.run()
            searchServiceProcess = process
        } catch {
            message = "Search service failed: \(error.localizedDescription)"
        }
    }

    func stopSearchService() {
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
        guard status?.roots.isEmpty == false else { return }
        if let watchProcess, watchProcess.isRunning { return }

        let process = Process()
        process.executableURL = cli.executableURL
        process.arguments = ["watch"]

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
        process.terminationHandler = { [weak self] _ in
            DispatchQueue.main.async {
                self?.watchProcess = nil
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
        hotKey?.unregister()
        stopSearchService()
        stopWatcher()
    }

    private func scheduleSearch() {
        pendingSearch?.cancel()
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            results = []
            selectedPath = nil
            isSearching = false
            return
        }

        let item = DispatchWorkItem { [weak self] in
            self?.performSearch(trimmed)
        }
        pendingSearch = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.12, execute: item)
    }

    private func performSearch(_ query: String) {
        searchGeneration += 1
        let generation = searchGeneration
        isSearching = true
        startSearchServiceIfNeeded()
        performServiceSearch(query, generation: generation)
    }

    private func performServiceSearch(_ query: String, generation: Int) {
        guard let url = serviceSearchURL(for: query) else {
            performCLISearch(query, generation: generation)
            return
        }

        URLSession.shared.dataTask(with: url) { data, response, error in
            if error != nil {
                self.performCLISearch(query, generation: generation)
                return
            }
            if let status = (response as? HTTPURLResponse)?.statusCode, status != 200 {
                self.performCLISearch(query, generation: generation)
                return
            }
            guard let data else {
                self.performCLISearch(query, generation: generation)
                return
            }

            do {
                let decoded = try JSONDecoder().decode([SearchResult].self, from: data)
                DispatchQueue.main.async {
                    guard generation == self.searchGeneration else { return }
                    self.isSearching = false
                    let sorted = self.sortedResults(decoded)
                    self.results = sorted
                    self.selectedPath = sorted.first?.path
                    self.message = "\(decoded.count) results • service"
                }
            } catch {
                self.performCLISearch(query, generation: generation)
            }
        }.resume()
    }

    private func performCLISearch(_ query: String, generation: Int) {
        DispatchQueue.global(qos: .userInitiated).async {
            let output: CommandOutput
            do {
                output = try self.cli.run(["search", query, "--limit", "300", "--json"])
            } catch {
                DispatchQueue.main.async {
                    guard generation == self.searchGeneration else { return }
                    self.isSearching = false
                    self.message = "Search failed: \(error.localizedDescription)"
                }
                return
            }

            DispatchQueue.main.async {
                guard generation == self.searchGeneration else { return }
                self.isSearching = false
                guard output.exitCode == 0 else {
                    self.message = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return
                }
                do {
                    let decoded = try JSONDecoder().decode([SearchResult].self, from: Data(output.stdout.utf8))
                    let sorted = self.sortedResults(decoded)
                    self.results = sorted
                    self.selectedPath = sorted.first?.path
                    self.message = "\(decoded.count) results"
                } catch {
                    self.message = "Search decode failed: \(error.localizedDescription)"
                }
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
        [
            "node_modules",
            ".git",
            "target",
            "dist",
            "build",
            ".build",
            ".Trash",
            "Library",
            "Library/Caches",
            "Library/Developer",
            "Library/Application Support",
            "Library/Containers",
            "Library/Group Containers",
            "Library/Mail",
            "Library/Messages",
            ".cache",
            ".npm",
            ".cargo/registry",
            "DerivedData",
            ".DS_Store"
        ].joined(separator: "\n")
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
                    model.refreshStatus()
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

struct ContentView: View {
    @EnvironmentObject private var model: SearchViewModel
    @State private var showingSettings = false

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
                .frame(width: 680, height: 520)
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
                MetricLine(label: "Entries", value: SearchViewModel.formatCount(model.status?.total ?? 0))
                MetricLine(label: "Files", value: SearchViewModel.formatCount(model.status?.files ?? 0))
                MetricLine(label: "Folders", value: SearchViewModel.formatCount(model.status?.dirs ?? 0))
                MetricLine(label: "Apps", value: SearchViewModel.formatCount(model.status?.apps ?? 0))
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
        VStack(spacing: 0) {
            ResultHeader(
                sortColumn: model.sortColumn,
                sortAscending: model.sortAscending,
                onSort: model.sort
            )
            if model.results.isEmpty {
                EmptyResultsView(
                    hasQuery: !model.query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                    hasIndex: (model.status?.total ?? 0) > 0,
                    isIndexing: model.isIndexing,
                    onIndex: { model.rebuildIndex() }
                )
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(model.results) { result in
                            ResultRow(
                                result: result,
                                isSelected: result.path == model.selectedPath
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
                }
            }
        }
        .background(Color(nsColor: .textBackgroundColor))
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
    let sortColumn: ResultSortColumn
    let sortAscending: Bool
    let onSort: (ResultSortColumn) -> Void

    var body: some View {
        HStack(spacing: 12) {
            HeaderCell(
                column: .name,
                width: 260,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort
            )
                .frame(width: 260, alignment: .leading)
            HeaderCell(
                column: .path,
                width: nil,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort
            )
            .frame(maxWidth: .infinity, alignment: .leading)
            HeaderCell(
                column: .kind,
                width: 74,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort
            )
            .frame(width: 74, alignment: .leading)
            HeaderCell(
                column: .size,
                width: 96,
                alignment: .trailing,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort
            )
            .frame(width: 96, alignment: .trailing)
            HeaderCell(
                column: .modified,
                width: 178,
                alignment: .leading,
                sortColumn: sortColumn,
                sortAscending: sortAscending,
                onSort: onSort
            )
            .frame(width: 178, alignment: .leading)
        }
        .font(.system(size: 11, weight: .semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Color(nsColor: .controlBackgroundColor))
    }
}

struct HeaderCell: View {
    let column: ResultSortColumn
    let width: CGFloat?
    let alignment: Alignment
    let sortColumn: ResultSortColumn
    let sortAscending: Bool
    let onSort: (ResultSortColumn) -> Void

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
            .frame(maxWidth: width ?? .infinity, alignment: alignment)
        }
        .buttonStyle(.plain)
        .help("Sort by \(column.title)")
    }
}

struct ResultRow: View {
    let result: SearchResult
    let isSelected: Bool

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
            .frame(width: 260, alignment: .leading)

            Text(result.path)
                .lineLimit(1)
                .truncationMode(.middle)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(result.kind)
                .foregroundStyle(.secondary)
                .frame(width: 74, alignment: .leading)
            Text(formatSize(result.size))
                .foregroundStyle(.secondary)
                .fontDesign(.monospaced)
                .frame(width: 96, alignment: .trailing)
            Text(formatDate(result.mtime))
                .foregroundStyle(.secondary)
                .frame(width: 178, alignment: .leading)
        }
        .font(.system(size: 13))
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
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
