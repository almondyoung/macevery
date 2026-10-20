import AppKit
import CryptoKit
import Darwin
import Foundation
import SwiftUI

// One harness is compiled with each frozen production Swift source. It never
// calls bootstrap/status, launches a service/watcher, or opens a user database.
// The baseline's private date function is copied byte-for-byte by the Python
// runner; only its declaration is made callable. ResultRow itself is unchanged.
private struct PerfFailure: Error, CustomStringConvertible {
    let description: String
}

private func need(_ value: @autoclosure () -> Bool, _ message: String) throws {
    if !value() { throw PerfFailure(description: message) }
}

private func pump(until condition: () -> Bool, timeout: TimeInterval = 10) throws {
    let end = Date().addingTimeInterval(timeout)
    while !condition() && Date() < end {
        autoreleasepool {
            _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.001))
        }
    }
    try need(condition(), "Fixed-result model operation did not finish")
}

private func settle(_ seconds: TimeInterval) {
    let end = Date().addingTimeInterval(seconds)
    while Date() < end {
        autoreleasepool {
            _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
        }
    }
}

private func fixedRows() -> [SearchResult] {
    let kinds: [String] = ["file", "dir", "app", "symlink"]
    return (0..<300).map { (index: Int) -> SearchResult in
        let basename = "\(index)-文件-ΟΣ.swift"
        let path = "/__macevery_round8_fixture__/\(basename)"
        let size: Int64? = index % 7 == 0 ? nil : Int64(index * 4096)
        let mtime: Int64? = index % 11 == 0 ? nil : Int64(1_700_000_000 + index * 3600)
        return SearchResult(path: path, basename: basename, kind: kinds[index % kinds.count],
                            size: size, mtime: mtime, score: Int64(index))
    }
}

private func jsonNumber(_ value: Int64?) -> Any {
    if let value { return NSNumber(value: value) }
    return NSNull()
}

private func resultData(_ values: [SearchResult]) throws -> Data {
    let objects: [[String: Any]] = values.map { value in
        ["path": value.path, "basename": value.basename, "kind": value.kind,
         "size": jsonNumber(value.size), "mtime": jsonNumber(value.mtime), "score": value.score]
    }
    return try JSONSerialization.data(withJSONObject: objects, options: [.sortedKeys])
}

private func digest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

private func emit(_ value: [String: Any], acknowledgement: Bool = true) throws {
    var data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    data.append(10)
    FileHandle.standardOutput.write(data)
    if acknowledgement {
        try need(readLine() == "continue", "Controller acknowledgement missing")
    }
}

private struct CPUReading {
    let user: Double
    let system: Double
    let peakRSS: Int64

    static func take() throws -> CPUReading {
        var value = rusage()
        try need(getrusage(RUSAGE_SELF, &value) == 0, "getrusage failed")
        let user = Double(value.ru_utime.tv_sec) + Double(value.ru_utime.tv_usec) / 1_000_000
        let system = Double(value.ru_stime.tv_sec) + Double(value.ru_stime.tv_usec) / 1_000_000
        return CPUReading(user: user, system: system, peakRSS: Int64(value.ru_maxrss))
    }
}

private func phase(_ name: String, operation: () throws -> [String: Any]) throws {
    try emit(["event": "phase_begin", "phase": name])
    let before = try CPUReading.take()
    let started = ProcessInfo.processInfo.systemUptime
    let details = try operation()
    let wall = ProcessInfo.processInfo.systemUptime - started
    let after = try CPUReading.take()
    try emit(["event": "phase_end", "phase": name, "details": details,
              "wall_seconds": wall, "user_cpu_seconds": after.user - before.user,
              "system_cpu_seconds": after.system - before.system,
              "cpu_seconds": after.user + after.system - before.user - before.system,
              "lifetime_peak_rss_bytes_at_phase_end": after.peakRSS])
}

private final class FixedResultProtocol: URLProtocol, @unchecked Sendable {
    static let payload = try! resultData(fixedRows())
    private let lock = NSLock()
    private var cancelled = false
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        guard let url = request.url, url.host == "127.0.0.1", url.port == 17649,
              url.path == "/search" else {
            client?.urlProtocol(self, didFailWithError: URLError(.unsupportedURL))
            return
        }
        lock.lock()
        let stopped = cancelled
        lock.unlock()
        guard !stopped else { return }
        let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Self.payload)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() { lock.lock(); cancelled = true; lock.unlock() }
}

private func displayedDate(_ value: Int64?) -> String {
#if MACEVERY_RESOURCE_BASELINE
    BaselineDisplayAdapter.date(value)
#else
    MacEveryDisplayFormatters.shared.date(value)
#endif
}

private struct RenderRows: View {
    let values: [SearchResult]
    let selected: Int
    var body: some View {
        let widths = ResultColumnWidths.defaults
        VStack(alignment: .leading, spacing: 0) {
            ForEach(values) { value in
                ResultRow(result: value, isSelected: value.score == Int64(selected),
                          widths: widths, tableWidth: widths.contentWidth)
            }
        }
        .frame(width: widths.contentWidth, height: 420, alignment: .topLeading)
    }
}

private struct Arguments {
    var formatterRounds = 20
    var modelRounds = 30
    var renderRounds = 75
    var settleSeconds: TimeInterval = 2
    var render = "offscreen"
    var timeZone = "UTC"
    init(_ values: [String]) throws {
        var index = 0
        while index < values.count {
            try need(index + 1 < values.count, "Each option requires a value")
            let key = values[index]
            let value = values[index + 1]
            switch key {
            case "--formatter-rounds": formatterRounds = Int(value) ?? 0
            case "--model-rounds": modelRounds = Int(value) ?? 0
            case "--render-rounds": renderRounds = Int(value) ?? 0
            case "--settle-seconds": settleSeconds = Double(value) ?? -1
            case "--render": render = value
            case "--time-zone": timeZone = value
            // Foundation reads these process-only argument-domain overrides.
            // No preferences are written, and the installed App is unaffected.
            case "-AppleLocale", "-AppleLanguages": break
            default: throw PerfFailure(description: "Unexpected isolated harness option")
            }
            index += 2
        }
        try need((1...10_000).contains(formatterRounds) && (1...10_000).contains(modelRounds)
                 && (1...10_000).contains(renderRounds), "Round count outside bound")
        try need((0...30).contains(settleSeconds) && ["offscreen", "none"].contains(render),
                 "Invalid settle/render option")
    }
}

@main
private struct GUIResourcePerfChecks {
    static func main() {
        do {
            let options = try Arguments(Array(CommandLine.arguments.dropFirst()))
            guard let zone = TimeZone(identifier: options.timeZone) else {
                throw PerfFailure(description: "Invalid time zone")
            }
            NSTimeZone.default = zone
            let values = fixedRows()
            let orderedDigest = digest(try resultData(values))
            _ = FixedResultProtocol.payload
            try emit(["event": "manifest", "rows": values.count, "ordered_six_field_digest": orderedDigest,
                      "locale_identifier": Locale.current.identifier,
                      "time_zone_identifier": TimeZone.current.identifier,
                      "calendar_identifier": String(describing: Calendar.current.identifier),
                      "formatter_rounds": options.formatterRounds, "model_rounds": options.modelRounds,
                      "render_rounds": options.renderRounds, "render": options.render,
                      "settle_seconds": options.settleSeconds, "visible_render_rows": 12])
            try phase("idle_before") { settle(options.settleSeconds); return [:] }
            try phase("formatters") {
                var checksum = 0
                var displayed: [[String]] = []
                for round in 0..<options.formatterRounds {
                    autoreleasepool {
                        for value in values {
                            let date = displayedDate(value.mtime)
                            let count = SearchViewModel.formatCount(value.size ?? 0)
                            checksum += date.utf8.count + count.utf8.count
                            if round == options.formatterRounds - 1 { displayed.append([date, count]) }
                        }
                    }
                }
                let data = try JSONSerialization.data(withJSONObject: displayed, options: [.sortedKeys])
                return ["display_digest": digest(data), "formatted_rows": values.count * options.formatterRounds,
                        "utf8_length_checksum": checksum]
            }

            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [FixedResultProtocol.self]
            configuration.urlCache = nil
            let session = URLSession(configuration: configuration)
            var model: SearchViewModel? = SearchViewModel(
                cli: MacEveryCLI(executableURL: URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL),
                searchSession: session, managesChildProcesses: false, searchDebounceDelay: 0)
            weak var weakModel = model
            try phase("model_replace_clear") {
                for index in 0..<options.modelRounds {
                    try autoreleasepool {
                        model!.query = "round8-fixed-\(index)"
                        try pump(until: { model!.results.count == 300 && !model!.isSearching })
                        try need(model!.results == values, "Model changed ordered six-field results")
                        model!.query = ""
                        try need(model!.results.isEmpty && !model!.isSearching, "Clear retained stale results")
                    }
                }
                return ["ordered_six_field_digest": orderedDigest, "replacements": options.modelRounds,
                        "clears": options.modelRounds, "rows_per_replacement": values.count]
            }

            var host: NSHostingView<RenderRows>?
            if options.render == "offscreen" {
                try phase("result_row_offscreen") {
                    NSApplication.shared.setActivationPolicy(.prohibited)
                    autoreleasepool {
                        host = NSHostingView(rootView: RenderRows(values: [], selected: -1))
                        host!.frame = NSRect(x: 0, y: 0, width: ResultColumnWidths.defaults.contentWidth, height: 420)
                    }
                    var pixelBytes = 0
                    for frame in 0..<options.renderRounds {
                        try autoreleasepool {
                            var page: [SearchResult] = []
                            for offset in 0..<12 { page.append(values[(frame * 12 + offset) % values.count]) }
                            host!.rootView = RenderRows(values: page, selected: frame % values.count)
                            host!.layoutSubtreeIfNeeded()
                            guard let bitmap = host!.bitmapImageRepForCachingDisplay(in: host!.bounds) else {
                                throw PerfFailure(description: "Offscreen bitmap unavailable; no silent render fallback")
                            }
                            host!.cacheDisplay(in: host!.bounds, to: bitmap)
                            try need(bitmap.pixelsWide > 0 && bitmap.pixelsHigh > 0, "Empty offscreen rendering")
                            pixelBytes += bitmap.bytesPerRow * bitmap.pixelsHigh
                        }
                    }
                    return ["frames": options.renderRounds, "rows_per_frame": 12,
                            "rendered_row_visits": options.renderRounds * 12, "bitmap_byte_visits": pixelBytes]
                }
            }
            try phase("idle_after_work") {
                settle(options.settleSeconds)
                return ["state": "model cleared; final 12-row host page if enabled; fixed 300-row fixture and JSON retained"]
            }
            weak var weakHost = host
            try phase("release_and_settle") {
                host = nil
                model = nil
                session.invalidateAndCancel()
                settle(options.settleSeconds)
                try need(weakModel == nil && weakHost == nil, "Harness retained released model or hosting view")
                return ["model_released": true, "hosting_view_released": true]
            }
            try emit(["event": "done", "ordered_six_field_digest": orderedDigest,
                      "model_released": weakModel == nil, "hosting_view_released": weakHost == nil])
        } catch {
            FileHandle.standardError.write(Data("FAIL: \(error)\n".utf8))
            Darwin.exit(1)
        }
    }
}
