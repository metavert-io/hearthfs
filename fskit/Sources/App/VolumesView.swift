// The Volumes section of the onboarding window: configured external sources
// (local folders, Obsidian/Logseq/GBrain vaults, Hearthloom, GitHub, …) with
// mount/unmount/remove, plus the add-volume wizard. Everything renders from
// the CLI's adapter registry (`hearthfs adapters --json`) — new adapters
// appear here with zero UI changes.

import SwiftUI

struct VolumesSection: View {
    @ObservedObject var model: VolumesModel
    /// False when embedded inside the onboarding checklist's mount step
    /// (which supplies its own title and description).
    var showHeader = true
    @State private var showWizard = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                if showHeader {
                    Text("Volumes").font(.headline)
                }
                Spacer()
                Button {
                    showWizard = true
                } label: {
                    Label("Add Volume…", systemImage: "plus")
                }
                .disabled(model.cliPath == nil)
            }
            if showHeader {
                Text("Mount external sources — notes vaults, second brains, GitHub — "
                     + "as searchable read-only volumes. Also available in a terminal: "
                     + "`hearthfs volume …`.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if model.cliPath == nil {
                Label(
                    "hearthfs CLI not found — install it (cargo install --path "
                    + "crates/hearthfs-cli), or point the app at a build with "
                    + "`defaults write \(Bundle.main.bundleIdentifier ?? "<app id>") "
                    + "\(VolumesModel.cliPathDefaultsKey) /path/to/hearthfs`, "
                    + "to manage volumes.",
                    systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }

            ForEach(model.volumes) { volume in
                volumeRow(volume)
            }

            if !model.lastError.isEmpty {
                Text(model.lastError)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }
        }
        .sheet(isPresented: $showWizard) {
            AddVolumeWizard(model: model)
        }
    }

    private func volumeRow(_ volume: VolumesModel.VolumeInfo) -> some View {
        HStack {
            Image(systemName: volume.mounted
                  ? "externaldrive.connected.to.line.below.fill" : "externaldrive")
                .foregroundStyle(volume.mounted ? .green : .secondary)
            VStack(alignment: .leading) {
                HStack(spacing: 6) {
                    Text(volume.name).font(.body)
                    Text(volume.adapter)
                        .font(.caption2)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(.quaternary, in: Capsule())
                }
                Text(volume.mountpoint).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if volume.mounted {
                Button("Reveal") {
                    NSWorkspace.shared.selectFile(
                        nil, inFileViewerRootedAtPath: volume.mountpoint)
                }
                Button("Unmount") { model.unmount(volume) }
                    .disabled(model.isWorking)
            } else {
                Button("Mount") { model.mount(volume) }
                    .disabled(model.isWorking)
                Button(role: .destructive) {
                    model.remove(volume)
                } label: {
                    Image(systemName: "trash")
                }
                .disabled(model.isWorking)
            }
        }
        .padding(8)
        .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
    }
}

// MARK: - Add-volume wizard

struct AddVolumeWizard: View {
    @ObservedObject var model: VolumesModel
    @Environment(\.dismiss) private var dismiss

    @State private var adapter: VolumesModel.AdapterInfo?
    @State private var fieldValues: [String: String] = [:]
    @State private var secretValue = ""
    @State private var volumeName = ""
    @State private var transport = ""
    @State private var isAdding = false
    @State private var error = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let adapter {
                configForm(adapter)
            } else {
                adapterPicker
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    // Step 1 — choose what to mount.
    private var adapterPicker: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add a Volume").font(.title3).bold()
            Text("Choose a source type:").foregroundStyle(.secondary)
            ForEach(model.adapters) { candidate in
                Button {
                    select(candidate)
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(candidate.displayName).font(.headline)
                        Text(candidate.description)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.leading)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
            }
        }
    }

    private func select(_ candidate: VolumesModel.AdapterInfo) {
        adapter = candidate
        transport = candidate.defaultTransport
        fieldValues = [:]
        for field in candidate.fields {
            if let def = field.defaultValue {
                fieldValues[field.key] = def
            }
        }
    }

    // Step 2 — adapter-specific configuration.
    private func configForm(_ adapter: VolumesModel.AdapterInfo) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add \(adapter.displayName)").font(.title3).bold()

            ForEach(adapter.fields) { field in
                fieldRow(field)
            }

            if let secret = adapter.secret, !secret.configured {
                VStack(alignment: .leading, spacing: 4) {
                    Text("API key").foregroundStyle(.secondary)
                    SecureField("Paste key", text: $secretValue)
                        .textFieldStyle(.roundedBorder)
                    Text(secret.hint).font(.caption).foregroundStyle(.secondary)
                }
            }

            HStack {
                Text("Name").frame(width: 96, alignment: .leading)
                    .foregroundStyle(.secondary)
                TextField("(auto)", text: $volumeName)
                    .textFieldStyle(.roundedBorder)
            }

            if adapter.transports.count > 1 {
                HStack {
                    Text("Transport").frame(width: 96, alignment: .leading)
                        .foregroundStyle(.secondary)
                    Picker("", selection: $transport) {
                        ForEach(adapter.transports, id: \.self) { Text($0).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                }
            }

            if !error.isEmpty {
                Text(error)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }

            HStack {
                Button("Back") { self.adapter = nil; error = "" }
                Spacer()
                Button("Cancel") { dismiss() }
                Button {
                    submit(adapter)
                } label: {
                    if isAdding {
                        ProgressView().controlSize(.small)
                    } else {
                        Text("Add & Mount")
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isAdding || !requiredFieldsFilled(adapter))
            }
        }
    }

    private func fieldRow(_ field: VolumesModel.FieldInfo) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(field.label).frame(width: 96, alignment: .leading)
                    .foregroundStyle(.secondary)
                if field.kind == "path" {
                    Text(fieldValues[field.key] ?? "Not set")
                        .font(.system(.body, design: .monospaced))
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button("Choose…") {
                        if let picked = OnboardingModel.pickDirectory(
                            title: "Choose \(field.label.lowercased())") {
                            fieldValues[field.key] = picked
                        }
                    }
                } else {
                    TextField(field.label, text: binding(for: field.key))
                        .textFieldStyle(.roundedBorder)
                }
            }
            Text(field.help).font(.caption).foregroundStyle(.secondary)
                .padding(.leading, 100)
        }
    }

    private func binding(for key: String) -> Binding<String> {
        Binding(
            get: { fieldValues[key] ?? "" },
            set: { fieldValues[key] = $0 })
    }

    private func requiredFieldsFilled(_ adapter: VolumesModel.AdapterInfo) -> Bool {
        for field in adapter.fields where field.required {
            if (fieldValues[field.key] ?? "").isEmpty { return false }
        }
        if let secret = adapter.secret, !secret.configured, secretValue.isEmpty {
            return false
        }
        return true
    }

    private func submit(_ adapter: VolumesModel.AdapterInfo) {
        isAdding = true
        error = ""
        Task {
            if let secret = adapter.secret, !secret.configured {
                guard await model.storeKey(service: secret.service, key: secretValue) else {
                    error = model.lastError
                    isAdding = false
                    return
                }
            }
            let ok = await model.addVolume(
                adapter: adapter,
                fieldValues: fieldValues,
                name: volumeName,
                transport: transport,
                mountNow: true)
            isAdding = false
            if ok {
                dismiss()
            } else {
                error = model.lastError
            }
        }
    }
}
