// The HearthFS onboarding window: a live checklist (installed → extension
// enabled → mounted) with a one-click action for each step, replacing the old
// static instruction text.

import SwiftUI

struct OnboardingView: View {
    @StateObject private var model = OnboardingModel()
    @StateObject private var volumesModel = VolumesModel()
    @State private var showMountHelp = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                header
                Divider()

                if !model.osMeetsMinimum {
                    osGateBanner
                }

                StepRow(
                    number: 1,
                    title: "Install HearthFS in Applications",
                    status: model.installStatus,
                    detail: model.installedInApplications
                        ? "Running from /Applications."
                        : "HearthFS should live in /Applications so macOS trusts its extension."
                ) {
                    if !model.installedInApplications {
                        Button("Move to Applications", action: model.moveToApplications)
                            .disabled(model.isWorking)
                    }
                }

                StepRow(
                    number: 2,
                    title: "Enable the File System extension",
                    status: model.enableStatus,
                    detail: model.enablementDetail
                ) {
                    HStack {
                        Button("Open File System Extensions…", action: model.openExtensionSettings)
                        Button("Refresh", action: model.refresh)
                            .disabled(model.isRefreshing)
                    }
                }

                StepRow(
                    number: 3,
                    title: "Mount your sources",
                    status: overallMountStatus,
                    detail: "Add volumes — folders, notes vaults, second brains, GitHub, "
                        + "MCP servers — and mount them as smart, searchable, read-only "
                        + "views. Your files stay put; nothing is copied or modified."
                ) {
                    VStack(alignment: .leading, spacing: 8) {
                        Button {
                            showMountHelp = true
                        } label: {
                            Label("Explain more", systemImage: "questionmark.circle")
                        }
                        .buttonStyle(.link)
                        .popover(isPresented: $showMountHelp, arrowEdge: .bottom) {
                            MountHelpView()
                        }
                        VolumesSection(model: volumesModel, showHeader: false)
                    }
                }

                if !model.activeMounts.isEmpty {
                    activeMountsSection
                }

                if !model.lastActionLog.isEmpty {
                    logView
                }

                Divider()
                footnote
            }
            .padding(24)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(minWidth: 620, minHeight: 560)
        .onAppear {
            model.refresh()
            Task { await volumesModel.refresh() }
        }
        // Re-check status whenever the user returns to the app — e.g. after
        // toggling the extension in System Settings — so the checklist keeps up
        // without a manual Refresh.
        .onReceive(NotificationCenter.default.publisher(
            for: NSApplication.didBecomeActiveNotification)) { _ in
            model.refresh()
            Task { await volumesModel.refresh() }
        }
    }

    /// Step 3 completes when anything is mounted through either path:
    /// configured volumes (any transport) or raw FSKit mounts.
    private var overallMountStatus: OnboardingModel.StepStatus {
        if !model.activeMounts.isEmpty || volumesModel.anyVolumeMounted {
            return .done
        }
        return model.mountStatus
    }

    // MARK: Sections

    /// Hard gate shown above the checklist on macOS < 26.5.2, where a system
    /// bug hangs third-party FSKit mounts. The Mount button is also disabled
    /// (canMount) so the user can't reach the mystery hang.
    private var osGateBanner: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.title2)
                .foregroundStyle(.orange)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 6) {
                Text("Update macOS before continuing").font(.headline)
                Text("HearthFS needs macOS \(OnboardingModel.minimumOSString) or later — you have "
                     + "\(model.osVersionString). Earlier versions of macOS 26 have a system bug "
                     + "that makes mounting hang. Mounting is disabled until you update.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Open Software Update…", action: model.openSoftwareUpdate)
                    .padding(.top, 2)
            }
            Spacer(minLength: 0)
        }
        .padding(12)
        .background(.orange.opacity(0.12), in: RoundedRectangle(cornerRadius: 10))
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("HearthFS")
                .font(.largeTitle).bold()
            Text("A searchable file system for your folders. Finish the three steps below "
                 + "to mount a folder as a HearthFS volume.")
                .foregroundStyle(.secondary)
        }
    }

    private var activeMountsSection: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Mounted volumes").font(.headline)
            ForEach(model.activeMounts) { mount in
                HStack {
                    Image(systemName: "externaldrive.connected.to.line.below.fill")
                        .foregroundStyle(.green)
                    VStack(alignment: .leading) {
                        Text(mount.path).font(.body)
                        Text(mount.source).font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Reveal") {
                        NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: mount.path)
                    }
                    Button("Unmount") { model.unmount(mount) }
                        .disabled(model.isWorking)
                }
                .padding(8)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            }
        }
    }

    private var logView: some View {
        Text(model.lastActionLog)
            .font(.system(.caption, design: .monospaced))
            .foregroundStyle(.secondary)
            .textSelection(.enabled)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
    }

    private var footnote: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Trouble enabling the extension?").font(.subheadline).bold()
            Text("On macOS 26.3.x, third-party FSKit modules can hang at mount because of a known "
                 + "system bug that Apple fixes in later point releases. If step 2 stays incomplete "
                 + "after enabling in Settings, update to macOS 26.5 or later and try again.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

// MARK: - "Explain more" help

/// Plain-language explanation of the mount step, aimed at AI enthusiasts rather
/// than developers: what to point HearthFS at, why, and what the mount point is.
private struct MountHelpView: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("What mounting does")
                    .font(.title3).bold()
                Text("HearthFS takes a folder you already have and adds a second, smart view "
                     + "of it. The originals are never changed or moved — the view is "
                     + "read-only. Behind the scenes HearthFS indexes the folder (full-text "
                     + "plus meaning-based search) so an AI assistant can find and read exactly "
                     + "the right passages instead of loading whole files.")

                HelpSection(
                    title: "The source folder — what to pick",
                    text: "Choose a folder holding the material you want an assistant to work "
                        + "with: notes, documents, research, code, saved emails. Good examples: "
                        + "an Obsidian or notes vault, a project folder, a downloads-of-papers "
                        + "folder, an exported email archive.")

                HelpSection(
                    title: "Why do this?",
                    text: "Pointing your AI tool at the HearthFS view lets it answer from your "
                        + "own material with far fewer tokens — it pulls the relevant snippets on "
                        + "demand rather than you pasting files in. Bigger, messier folders "
                        + "benefit most. HearthFS automatically skips clutter like caches, "
                        + ".git, and node_modules.")

                HelpSection(
                    title: "The mount point — what to know",
                    text: "The mount point is where the smart view appears — think of it as a "
                        + "window, not a copy. Nothing is duplicated, and it holds no data of "
                        + "its own. HearthFS suggests one under /Volumes (the standard spot for "
                        + "mounted volumes, so it shows up in Finder's sidebar), named after "
                        + "your source folder. When you unmount, the window closes and your "
                        + "source folder is completely untouched.")

                HelpSection(
                    title: "Once it's mounted",
                    text: "Browse the mount point like any folder. It also exposes magic paths: "
                        + "“@search” finds the most relevant files for a query, and “@fit” fills "
                        + "a token budget with the best content. Point your AI assistant at the "
                        + "mount point and it can use these directly.")
            }
            .padding(20)
        }
        .frame(width: 380, height: 460)
    }
}

private struct HelpSection: View {
    let title: String
    let text: String
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.headline)
            Text(text).foregroundStyle(.secondary)
        }
    }
}

// MARK: - Reusable rows

private struct StepRow<Actions: View>: View {
    let number: Int
    let title: String
    let status: OnboardingModel.StepStatus
    let detail: String
    @ViewBuilder var actions: () -> Actions

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: status.symbol)
                .font(.title2)
                .foregroundStyle(status.tint)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 6) {
                Text("\(number). \(title)").font(.headline)
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                actions()
                    .padding(.top, 2)
            }
            Spacer(minLength: 0)
        }
    }
}

