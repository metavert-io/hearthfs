// Drives the HearthFS onboarding window: live status for each setup step
// (installed → extension enabled → mounted) plus the actions that advance
// them. Deliberately non-sandboxed (the host app carries no app-sandbox
// entitlement) so it can shell out to /sbin/mount and read mount state.

import Foundation
import SwiftUI
import FSKit
import AppKit

@MainActor
final class OnboardingModel: ObservableObject {
    /// The FSKit module's bundle id, read from the extension embedded in this
    /// app (Contents/Extensions/*.appex at the fsmodule extension point).
    /// Bundle IDs come from each developer's HEARTHFS_BUNDLE_PREFIX
    /// (fskit/Local.xcconfig), so never hardcode one; the fallback mirrors
    /// project.yml's `$(HEARTHFS_BUNDLE_PREFIX).fskit`.
    static let extensionBundleID: String = {
        let extensions = Bundle.main.bundleURL.appendingPathComponent("Contents/Extensions")
        let appexes = (try? FileManager.default.contentsOfDirectory(
            at: extensions, includingPropertiesForKeys: nil)) ?? []
        for url in appexes where url.pathExtension == "appex" {
            guard let bundle = Bundle(url: url), let id = bundle.bundleIdentifier else { continue }
            let attributes = bundle.object(forInfoDictionaryKey: "EXAppExtensionAttributes")
                as? [String: Any]
            if attributes?["EXExtensionPointIdentifier"] as? String == "com.apple.fskit.fsmodule" {
                return id
            }
        }
        return (Bundle.main.bundleIdentifier ?? "") + ".fskit"
    }()
    /// The `mount -F -t <shortName>` type string (FSShortName, lowercase).
    static let shortName = "hearthfs"

    /// Earliest macOS verified to mount third-party FSKit path-URL modules.
    /// 26.3.x hangs at fskitd "Probe starting" (Apple platform bug, fixed in
    /// point releases) — gate hard so users see "update macOS" instead of a
    /// mystery hang.
    static let minimumOS = OperatingSystemVersion(majorVersion: 26, minorVersion: 5, patchVersion: 2)
    static let minimumOSString = "26.5.2"

    let osMeetsMinimum = ProcessInfo.processInfo.isOperatingSystemAtLeast(minimumOS)
    var osVersionString: String {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        return "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)"
    }

    enum StepStatus {
        case done, pending, blocked, unknown

        var symbol: String {
            switch self {
            case .done: return "checkmark.circle.fill"
            case .pending: return "circle"
            case .blocked: return "exclamationmark.triangle.fill"
            case .unknown: return "questionmark.circle"
            }
        }
        var tint: Color {
            switch self {
            case .done: return .green
            case .pending: return .secondary
            case .blocked: return .orange
            case .unknown: return .secondary
            }
        }
    }

    struct MountPoint: Identifiable, Hashable {
        let id = UUID()
        let source: String
        let path: String
    }

    // MARK: Published state

    @Published var installedInApplications = false
    @Published var extensionEnabled = false
    @Published var enablementDetail = "Checking…"
    @Published var activeMounts: [MountPoint] = []
    @Published var isRefreshing = false
    @Published var isWorking = false
    @Published var lastActionLog = ""

    @Published var sourceDir = UserDefaults.standard.string(forKey: "hearthfs.sourceDir") ?? ""
    /// Mount points default under /Volumes (the macOS standard for mounted
    /// volumes — this is what appears in Finder's sidebar). The leaf name is
    /// derived from the source folder and collision-checked, so we never point
    /// at, or clash with, a folder the user already has. Empty until a source
    /// is chosen; recomputed on source change unless the user set it by hand.
    @Published var mountDir = ""
    private var mountManuallySet = false

    // MARK: Derived step status

    var installStatus: StepStatus { installedInApplications ? .done : .pending }

    var enableStatus: StepStatus {
        if extensionEnabled { return .done }
        return installedInApplications ? .pending : .unknown
    }

    var mountStatus: StepStatus {
        if !activeMounts.isEmpty { return .done }
        return extensionEnabled ? .pending : .unknown
    }

    // MARK: Refresh

    func refresh() {
        isRefreshing = true
        installedInApplications = Bundle.main.bundlePath.hasPrefix("/Applications/")
        if !sourceDir.isEmpty && mountDir.isEmpty {
            mountDir = suggestedMountPoint(forSource: sourceDir)
        }
        refreshMounts()
        Task { await refreshEnablement() }
    }

    /// A `/Volumes/<name>` path derived from the source folder's name, bumped
    /// with a numeric suffix if anything already exists there (checked
    /// case-insensitively, since the default macOS volume is case-insensitive).
    func suggestedMountPoint(forSource source: String) -> String {
        let leaf = (source as NSString).lastPathComponent
        let base = (leaf.isEmpty || leaf == "/") ? "HearthFS" : leaf
        var candidate = "/Volumes/\(base)"
        var n = 2
        while FileManager.default.fileExists(atPath: candidate) {
            candidate = "/Volumes/\(base) \(n)"
            n += 1
        }
        return candidate
    }

    func refreshEnablement() async {
        do {
            let modules = try await FSClient.shared.installedExtensions
            if let mine = modules.first(where: { $0.bundleIdentifier == Self.extensionBundleID }) {
                extensionEnabled = mine.isEnabled
                enablementDetail = mine.isEnabled
                    ? "Enabled and visible to FSKit."
                    : "Installed but toggled off. Turn it on in System Settings."
            } else {
                extensionEnabled = false
                enablementDetail = "Not yet visible to FSKit. Enable it in System Settings, "
                    + "then press Refresh. (On macOS 26.3.x this can require the 26.5+ update — "
                    + "see the note below.)"
            }
        } catch {
            extensionEnabled = false
            enablementDetail = "Couldn't query FSKit: \(error.localizedDescription)"
        }
        isRefreshing = false
    }

    func refreshMounts() {
        let result = Self.run("/sbin/mount", [])
        activeMounts = result.stdout.split(separator: "\n").compactMap { rawLine in
            let line = String(rawLine)
            // Lines look like: "<source> on <path> (hearthfs, local, read-only, ...)"
            guard line.contains("(\(Self.shortName)") || line.contains("(\(Self.shortName),") else { return nil }
            guard let onRange = line.range(of: " on "),
                  let parenRange = line.range(of: " (") else { return nil }
            let source = String(line[line.startIndex..<onRange.lowerBound])
            let path = String(line[onRange.upperBound..<parenRange.lowerBound])
            return MountPoint(source: source, path: path)
        }
    }

    // MARK: Actions

    /// Deep-links straight into System Settings › General › Login Items &
    /// Extensions › File System Extensions (the fsmodule extension point pane).
    func openExtensionSettings() {
        let deepLink = "x-apple.systempreferences:com.apple.ExtensionsPreferences"
            + "?extensionPointIdentifier=com.apple.fskit.fsmodule"
        if let url = URL(string: deepLink) {
            NSWorkspace.shared.open(url)
        }
    }

    func openSoftwareUpdate() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preferences.softwareupdate") {
            NSWorkspace.shared.open(url)
        }
    }

    func chooseSourceDirectory() {
        if let picked = Self.pickDirectory(title: "Choose the folder to index and mount") {
            sourceDir = picked
            UserDefaults.standard.set(picked, forKey: "hearthfs.sourceDir")
            // Auto-suggest a matching mount point unless the user picked one.
            if !mountManuallySet {
                mountDir = suggestedMountPoint(forSource: picked)
            }
        }
    }

    func chooseMountDirectory() {
        if let picked = Self.pickDirectory(title: "Choose (or create) an empty mount point") {
            mountDir = picked
            mountManuallySet = true
        }
    }

    var canMount: Bool {
        osMeetsMinimum && !sourceDir.isEmpty && !mountDir.isEmpty && !isWorking
    }

    var mountCommand: String {
        "mount -F -t \(Self.shortName) \(shellQuote(sourceDir)) \(shellQuote(mountDir))"
    }

    func mount() {
        guard canMount else { return }
        UserDefaults.standard.set(sourceDir, forKey: "hearthfs.sourceDir")
        isWorking = true
        let source = sourceDir, mountpoint = mountDir
        let mountArgs = "-F -t \(Self.shortName) "
            + "\(Self.shellQuoteStatic(source)) \(Self.shellQuoteStatic(mountpoint))"
        // Creating a mount point under /Volumes (root:wheel) needs privilege;
        // fold the mkdir + mount into one admin prompt in that case.
        let parent = (mountpoint as NSString).deletingLastPathComponent
        let needsAdmin = !FileManager.default.isWritableFile(atPath: parent)
            && !FileManager.default.fileExists(atPath: mountpoint)
        Task.detached {
            var result: CommandResult
            if needsAdmin {
                result = OnboardingModel.runAdmin(
                    "mkdir -p \(OnboardingModel.shellQuoteStatic(mountpoint)) && mount \(mountArgs)")
            } else {
                try? FileManager.default.createDirectory(
                    atPath: mountpoint, withIntermediateDirectories: true)
                result = OnboardingModel.run(
                    "/sbin/mount", ["-F", "-t", OnboardingModel.shortName, source, mountpoint])
                if result.exitCode != 0 && OnboardingModel.looksLikePermission(result) {
                    result = OnboardingModel.runAdmin("mount \(mountArgs)")
                }
            }
            let log = result.exitCode == 0
                ? "Mounted \(source) → \(mountpoint)"
                : "mount failed (exit \(result.exitCode)):\n\(result.stderr)\(result.stdout)"
            await MainActor.run {
                self.lastActionLog = log
                self.isWorking = false
                self.refreshMounts()
            }
        }
    }

    func unmount(_ mp: MountPoint) {
        isWorking = true
        let path = mp.path
        let underVolumes = path.hasPrefix("/Volumes/")
        Task.detached {
            var result = OnboardingModel.run("/sbin/umount", [path])
            if result.exitCode != 0 {
                result = OnboardingModel.run("/usr/sbin/diskutil", ["unmount", path])
            }
            if result.exitCode != 0 {
                // Root-owned mount (created under /Volumes) needs admin to
                // detach; tidy the now-empty stub directory in the same prompt.
                let quoted = OnboardingModel.shellQuoteStatic(path)
                let cmd = underVolumes
                    ? "umount \(quoted) ; rmdir \(quoted) 2>/dev/null || true"
                    : "umount \(quoted)"
                result = OnboardingModel.runAdmin(cmd)
            } else if underVolumes {
                try? FileManager.default.removeItem(atPath: path) // best-effort stub cleanup
            }
            let log = result.exitCode == 0
                ? "Unmounted \(path)"
                : "umount failed (exit \(result.exitCode)):\n\(result.stderr)\(result.stdout)"
            await MainActor.run {
                self.lastActionLog = log
                self.isWorking = false
                self.refreshMounts()
            }
        }
    }

    /// Copies the running bundle into /Applications (admin-authorized) and
    /// relaunches from there. FSKit only vends extensions whose host app lives
    /// in a trusted location and has been launched at least once.
    func moveToApplications() {
        let src = Bundle.main.bundlePath
        guard !src.hasPrefix("/Applications/") else { return }
        let dest = "/Applications/\(Bundle.main.bundleURL.lastPathComponent)"
        isWorking = true
        Task.detached {
            let result = OnboardingModel.runAdmin(
                "rm -rf \(OnboardingModel.shellQuoteStatic(dest)) && "
                + "cp -R \(OnboardingModel.shellQuoteStatic(src)) "
                + "\(OnboardingModel.shellQuoteStatic(dest))")
            await MainActor.run {
                self.isWorking = false
                if result.exitCode == 0 {
                    self.lastActionLog = "Installed to \(dest). Relaunching…"
                    OnboardingModel.relaunch(at: dest)
                } else {
                    self.lastActionLog = "Move to /Applications failed:\n\(result.stderr)"
                }
            }
        }
    }

    // MARK: - Shell helpers

    struct CommandResult: Sendable { let stdout: String; let stderr: String; let exitCode: Int32 }

    nonisolated static func run(_ launchPath: String, _ arguments: [String]) -> CommandResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: launchPath)
        process.arguments = arguments
        let outPipe = Pipe(), errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe
        do {
            try process.run()
        } catch {
            return CommandResult(stdout: "", stderr: error.localizedDescription, exitCode: -1)
        }
        let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
        let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return CommandResult(
            stdout: String(decoding: outData, as: UTF8.self),
            stderr: String(decoding: errData, as: UTF8.self),
            exitCode: process.terminationStatus)
    }

    /// Runs a shell command with an administrator authorization prompt.
    nonisolated static func runAdmin(_ shellCommand: String) -> CommandResult {
        let escaped = shellCommand.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        let script = "do shell script \"\(escaped)\" with administrator privileges"
        return run("/usr/bin/osascript", ["-e", script])
    }

    nonisolated static func looksLikePermission(_ result: CommandResult) -> Bool {
        let text = (result.stderr + result.stdout).lowercased()
        return text.contains("permission") || text.contains("not permitted")
            || text.contains("must be root") || text.contains("operation not permitted")
    }

    static func pickDirectory(title: String) -> String? {
        let panel = NSOpenPanel()
        panel.title = title
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        return panel.runModal() == .OK ? panel.url?.path : nil
    }

    static func relaunch(at bundlePath: String) {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        task.arguments = ["-n", bundlePath]
        try? task.run()
        NSApp.terminate(nil)
    }

    private func shellQuote(_ s: String) -> String { Self.shellQuoteStatic(s) }
    nonisolated static func shellQuoteStatic(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
