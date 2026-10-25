// Bridge between the app UI and the hearthfs CLI — the single source of
// truth for adapters and volume config. The UI shells out to the CLI for
// every operation (adapters --json, volume list/add/mount/…, auth), so
// anything configurable here is configurable from a terminal, and the two
// can never drift.

import Foundation
import SwiftUI

@MainActor
final class VolumesModel: ObservableObject {

    // MARK: CLI JSON models (shapes owned by `hearthfs adapters --json` /
    // `hearthfs volume list --json`)

    struct FieldInfo: Decodable, Identifiable {
        let key: String
        let label: String
        let kind: String // "path" | "url" | "text"
        let required: Bool
        let help: String
        let defaultValue: String?
        var id: String { key }
        enum CodingKeys: String, CodingKey {
            case key, label, kind, required, help
            case defaultValue = "default"
        }
    }

    struct SecretInfo: Decodable {
        let service: String
        let hint: String
        let configured: Bool
    }

    struct AdapterInfo: Decodable, Identifiable {
        let kind: String
        let displayName: String
        let description: String
        let fields: [FieldInfo]
        let secret: SecretInfo?
        let transports: [String]
        let defaultTransport: String
        var id: String { kind }
        enum CodingKeys: String, CodingKey {
            case kind, description, fields, secret, transports
            case displayName = "display_name"
            case defaultTransport = "default_transport"
        }
    }

    struct VolumeInfo: Decodable, Identifiable {
        let name: String
        let adapter: String
        let mountpoint: String
        let transport: String
        let mounted: Bool
        var id: String { name }
    }

    private struct AdaptersEnvelope: Decodable { let adapters: [AdapterInfo] }
    private struct VolumesEnvelope: Decodable { let volumes: [VolumeInfo] }

    // MARK: Published state

    @Published var adapters: [AdapterInfo] = []
    @Published var volumes: [VolumeInfo] = []

    var anyVolumeMounted: Bool { volumes.contains { $0.mounted } }
    @Published var cliPath: String?
    @Published var isWorking = false
    @Published var lastError = ""

    // MARK: CLI discovery + execution (off the main actor — a mount can
    // take seconds)

    /// UserDefaults key for an explicit CLI path, e.g. a development build:
    /// `defaults write <app bundle id> hearthfs.cliPath /path/to/hearthfs`.
    nonisolated static let cliPathDefaultsKey = "hearthfs.cliPath"

    /// Where the hearthfs CLI might live, best first: an explicitly configured
    /// path (the `hearthfs.cliPath` default, or $HEARTHFS_CLI), a bundled
    /// helper, then every directory on PATH plus the usual install folders
    /// (apps launched from Finder get a minimal PATH, so those are listed
    /// explicitly).
    nonisolated static func findCLI() -> String? {
        let fm = FileManager.default
        let configured = [
            UserDefaults.standard.string(forKey: cliPathDefaultsKey),
            ProcessInfo.processInfo.environment["HEARTHFS_CLI"],
        ].compactMap { $0 }.map { ($0 as NSString).expandingTildeInPath }
        let home = NSHomeDirectory()
        let pathDirs = (ProcessInfo.processInfo.environment["PATH"] ?? "")
            .split(separator: ":").map(String.init)
        let searchDirs = pathDirs + [
            "/usr/local/bin", "/opt/homebrew/bin", home + "/.cargo/bin", home + "/.local/bin",
        ]
        let candidates = configured
            + [Bundle.main.bundlePath + "/Contents/Helpers/hearthfs"]
            + searchDirs.map { $0 + "/hearthfs" }
        return candidates.first { fm.isExecutableFile(atPath: $0) }
    }

    nonisolated static func runCLI(
        _ cli: String, _ args: [String], stdin: String? = nil
    ) -> OnboardingModel.CommandResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: cli)
        process.arguments = args
        let outPipe = Pipe(), errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe
        if let stdin {
            let inPipe = Pipe()
            process.standardInput = inPipe
            inPipe.fileHandleForWriting.write(Data((stdin + "\n").utf8))
            inPipe.fileHandleForWriting.closeFile()
        }
        do {
            try process.run()
        } catch {
            return .init(stdout: "", stderr: error.localizedDescription, exitCode: -1)
        }
        let out = outPipe.fileHandleForReading.readDataToEndOfFile()
        let err = errPipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return .init(
            stdout: String(decoding: out, as: UTF8.self),
            stderr: String(decoding: err, as: UTF8.self),
            exitCode: process.terminationStatus)
    }

    private func runOffMain(
        _ args: [String], stdin: String? = nil
    ) async -> OnboardingModel.CommandResult {
        guard let cli = cliPath else {
            return .init(stdout: "", stderr: "hearthfs CLI not found", exitCode: -1)
        }
        return await Task.detached { Self.runCLI(cli, args, stdin: stdin) }.value
    }

    // MARK: Operations

    func refresh() async {
        cliPath = Self.findCLI()
        guard cliPath != nil else { return }
        let adaptersResult = await runOffMain(["adapters", "--json"])
        if adaptersResult.exitCode == 0,
           let data = adaptersResult.stdout.data(using: .utf8),
           let envelope = try? JSONDecoder().decode(AdaptersEnvelope.self, from: data) {
            adapters = envelope.adapters
        }
        let volumesResult = await runOffMain(["volume", "list", "--json"])
        if volumesResult.exitCode == 0,
           let data = volumesResult.stdout.data(using: .utf8),
           let envelope = try? JSONDecoder().decode(VolumesEnvelope.self, from: data) {
            volumes = envelope.volumes
        }
    }

    /// Store an adapter's API key via `hearthfs auth <service>` (stdin, so
    /// the secret never appears in argv or logs; the CLI writes it 0600).
    func storeKey(service: String, key: String) async -> Bool {
        let result = await runOffMain(["auth", service], stdin: key)
        if result.exitCode != 0 {
            lastError = result.stderr
        }
        return result.exitCode == 0
    }

    /// Add (and optionally mount) a volume. False + lastError on failure.
    func addVolume(
        adapter: AdapterInfo,
        fieldValues: [String: String],
        name: String,
        transport: String,
        mountNow: Bool
    ) async -> Bool {
        var args = ["volume", "add", adapter.kind]
        if !name.isEmpty { args += ["--name", name] }
        if transport != adapter.defaultTransport { args += ["--transport", transport] }
        for field in adapter.fields {
            if let value = fieldValues[field.key], !value.isEmpty {
                args += ["--field", "\(field.key)=\(value)"]
            }
        }
        let added = await runOffMain(args)
        guard added.exitCode == 0 else {
            lastError = added.stderr
            return false
        }
        if mountNow {
            // The CLI prints the assigned name; recover it for the mount.
            let assigned = name.isEmpty
                ? Self.parseAssignedName(from: added.stderr) : name
            if let assigned {
                let mounted = await runOffMain(["volume", "mount", assigned])
                if mounted.exitCode != 0 {
                    lastError = mounted.stderr
                    await refresh()
                    return false
                }
            }
        }
        await refresh()
        return true
    }

    /// "Added volume 'brain' (…" → "brain"
    nonisolated static func parseAssignedName(from output: String) -> String? {
        guard let start = output.range(of: "Added volume '") else { return nil }
        let rest = output[start.upperBound...]
        return rest.split(separator: "'").first.map(String.init)
    }

    func mount(_ volume: VolumeInfo) { perform(["volume", "mount", volume.name]) }
    func unmount(_ volume: VolumeInfo) { perform(["volume", "unmount", volume.name]) }
    func remove(_ volume: VolumeInfo) { perform(["volume", "remove", volume.name]) }

    private func perform(_ args: [String]) {
        isWorking = true
        lastError = ""
        Task {
            let result = await runOffMain(args)
            if result.exitCode != 0 {
                lastError = result.stderr
            }
            isWorking = false
            await refresh()
        }
    }
}
