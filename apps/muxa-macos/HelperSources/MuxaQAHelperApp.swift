import SwiftUI

@main
struct MuxaQAHelperApp: App {
    @StateObject private var model = QAHelperModel()

    var body: some Scene {
        WindowGroup("Muxa QA Helper") {
            QAHelperView(model: model)
                .frame(minWidth: 620, minHeight: 520)
        }
        .windowResizability(.contentMinSize)
    }
}

private struct QAHelperView: View {
    @ObservedObject var model: QAHelperModel

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 5) {
                Text("Muxa QA Helper")
                    .font(.largeTitle.bold())
                Text("Only captures and controls the Muxa app for local UI verification.")
                    .foregroundStyle(.secondary)
            }

            GroupBox("Permissions") {
                VStack(alignment: .leading, spacing: 10) {
                    permissionRow(
                        title: "Accessibility",
                        granted: model.permissions.accessibility,
                        detail: "Raises Muxa and sends test keyboard input."
                    )
                    Divider()
                    permissionRow(
                        title: "Screen Recording",
                        granted: model.permissions.screenRecording,
                        detail: "Captures only the onscreen Muxa window."
                    )
                    HStack {
                        Button("Request Permissions") { model.requestPermissions() }
                            .buttonStyle(.borderedProminent)
                        Button("Refresh") { model.refreshPermissions() }
                    }
                    .padding(.top, 4)
                }
                .padding(8)
            }

            GroupBox("Local QA service") {
                VStack(alignment: .leading, spacing: 7) {
                    LabeledContent("Socket") {
                        Text(model.server.socketPath)
                            .textSelection(.enabled)
                            .font(.system(.caption, design: .monospaced))
                    }
                    if let error = model.serverError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                    } else {
                        Label("Owner-only service is running", systemImage: "checkmark.shield.fill")
                            .foregroundStyle(.green)
                    }
                }
                .padding(8)
            }

            HStack {
                Button("Capture Muxa Preview") { model.capturePreview() }
                    .disabled(!model.permissions.screenRecording)
                Text(model.activityMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if let image = model.previewImage {
                Image(nsImage: image)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: .infinity, maxHeight: 220)
                    .background(Color.black.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            } else {
                Spacer(minLength: 0)
            }
        }
        .padding(22)
    }

    private func permissionRow(
        title: String,
        granted: Bool,
        detail: String
    ) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: granted ? "checkmark.circle.fill" : "xmark.circle.fill")
                .foregroundStyle(granted ? .green : .orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).fontWeight(.medium)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Text(granted ? "Granted" : "Required")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}
