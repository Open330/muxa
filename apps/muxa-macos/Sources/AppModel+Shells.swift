import Foundation

// MARK: - Native shell launches

/// Everything muxad needs to spawn one native shell, computed without the
/// daemon so the command, session name, and environment are unit-testable.
struct MuxaNativeShellLaunch: Equatable, Sendable {
    let command: String
    let arguments: [String]
    let cwd: String
    let name: String
    let environment: [String: String]

    /// Fleet host states that accept an interactive SSH shell from this Mac.
    /// `version_skew` only means the remote muxa differs; OpenSSH still works.
    static let remoteShellHostStates: Set<String> = ["online", "version_skew"]

    /// OpenSSH as shipped with macOS. Nil when the binary is missing so the
    /// launch can fall back to a PATH lookup through `/usr/bin/env`.
    static let systemSSHPath = "/usr/bin/ssh"

    /// Whether the Shells "+" menu should offer an SSH shell on `host`.
    static func canOpenRemoteShell(on host: MuxaFleetHost) -> Bool {
        !host.local && remoteShellHostStates.contains(host.state)
    }

    /// The `ssh` destination for a fleet host: the OpenSSH Host alias or
    /// `user@host` muxad itself dials (`ssh_target`), or the muxa alias when
    /// the snapshot carries no usable target.
    static func sshDestination(for host: MuxaFleetHost) -> String {
        let target = host.sshTarget?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !target.isEmpty, target != "local://" else { return host.alias }
        return target
    }

    /// Sidebar name of an SSH shell, e.g. "devbox shell".
    static func remoteShellName(hostAlias: String) -> String {
        String(localized: "\(hostAlias) shell")
    }

    /// The environment `AppModel.createShell()` gives a native PTY: names the
    /// terminal, records the owning shell, and forces UTF-8 when the login
    /// environment has no locale.
    static func terminalEnvironment(
        base: [String: String],
        shell: String,
        appVersion: String
    ) -> [String: String] {
        var environment = base
        environment["TERM"] = "xterm-256color"
        environment["COLORTERM"] = "truecolor"
        environment["TERM_PROGRAM"] = "Muxa"
        environment["TERM_PROGRAM_VERSION"] = appVersion
        environment["SHELL"] = shell
        if environment["LANG"]?.isEmpty != false {
            environment["LANG"] = "en_US.UTF-8"
        }
        if environment["LC_ALL"]?.isEmpty != false,
           environment["LC_CTYPE"]?.isEmpty != false {
            environment["LC_CTYPE"] = "en_US.UTF-8"
        }
        return environment
    }

    /// A native shell that runs `ssh -- <destination>` to a fleet host, the
    /// same destination form muxad uses for its relay.
    static func remoteShell(
        host: MuxaFleetHost,
        base: [String: String],
        shell: String,
        appVersion: String,
        home: String,
        sshExecutable: String?
    ) -> MuxaNativeShellLaunch {
        var environment = terminalEnvironment(base: base, shell: shell, appVersion: appVersion)
        // The app owns a fresh PTY. A development terminal's tmux markers
        // would make tools on the remote side believe they run inside tmux.
        environment.removeValue(forKey: "TMUX")
        environment.removeValue(forKey: "TMUX_PANE")
        let destination = sshDestination(for: host)
        let command: String
        let arguments: [String]
        if let sshExecutable {
            command = sshExecutable
            arguments = ["--", destination]
        } else {
            command = "/usr/bin/env"
            arguments = ["ssh", "--", destination]
        }
        return MuxaNativeShellLaunch(
            command: command,
            arguments: arguments,
            cwd: home,
            name: remoteShellName(hostAlias: host.alias),
            environment: environment
        )
    }
}

extension MuxaSession {
    /// One-line state for a Shells row: "Running", "2 attached", or the
    /// exit outcome once the process ended.
    var shellStateText: String {
        if exited {
            guard let exitStatus, exitStatus != 0 else { return String(localized: "Exited") }
            return String(localized: "Exited with status \(exitStatus)")
        }
        return attachedClients > 0
            ? String(localized: "\(attachedClients) attached")
            : String(localized: "Running")
    }
}

extension AppModel {
    /// Fleet hosts the Shells "+" menu can open an SSH shell on.
    var remoteShellHosts: [MuxaFleetHost] {
        fleetHosts
            .filter(MuxaNativeShellLaunch.canOpenRemoteShell(on:))
            .sorted { $0.alias < $1.alias }
    }

    /// Opens a native shell running `ssh` to `host`, named "<alias> shell",
    /// and selects it. Throws so the Shells sidebar can show the failure
    /// inline; `connectionState` stays owned by `AppModel.swift`.
    func createShell(sshHost host: MuxaFleetHost) async throws {
        guard isConnected else { return }
        let environment = ProcessInfo.processInfo.environment
        let shell = environment["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh"
        let appVersion = Bundle.main
            .object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "development"
        let sshExecutable = FileManager.default.isExecutableFile(
            atPath: MuxaNativeShellLaunch.systemSSHPath
        ) ? MuxaNativeShellLaunch.systemSSHPath : nil
        let launch = MuxaNativeShellLaunch.remoteShell(
            host: host,
            base: environment,
            shell: shell,
            appVersion: appVersion,
            home: FileManager.default.homeDirectoryForCurrentUser.path,
            sshExecutable: sshExecutable
        )
        do {
            let session = try await client.spawnShell(
                command: launch.command,
                arguments: launch.arguments,
                cwd: launch.cwd,
                name: launch.name,
                environment: launch.environment
            )
            registerSpawnedSession(session)
            await refresh()
        } catch {
            MuxaLog.app.error(
                "remote shell launch failed: \(error.localizedDescription, privacy: .public)"
            )
            throw error
        }
    }
}

// MARK: - Inbox "Needs attention" rows

extension MuxaHostedAgent {
    /// Live Watch identity of the pane this agent runs in, when it has one.
    var watchPaneIdentity: MuxaWatchPaneIdentity? {
        pane.map {
            MuxaWatchPaneIdentity(
                hostAlias: host.alias,
                socket: $0.endpointSocket,
                paneID: $0.paneID
            )
        }
    }

    /// Operator requests addressed to this agent, newest first, that still
    /// wait for the agent or carry a reply the operator has not read.
    func openRequests(in messages: [MuxaOperatorMessage]) -> [MuxaOperatorMessage] {
        messages
            .filter { message in
                message.host.alias == host.alias
                    && message.request.to.agentSessionID == agent.agentSessionID
                    && (message.needsReply || message.hasUnreadReply)
            }
            .sorted(by: MuxaOperatorMessage.needsActionOrder)
    }
}

extension AppModel {
    /// What an Inbox "Needs attention" row did before it became a selection:
    /// follow the agent's pane in Live Watch, or select the agent itself
    /// when it is not attached to a pane.
    func openInLiveWatch(_ participant: MuxaHostedAgent) {
        if let pane = participant.watchPaneIdentity {
            selectWatchPane(pane)
        } else {
            select(.agent(participant.id))
        }
    }

    /// The managed Work this agent belongs to, if any.
    func workGroup(for participant: MuxaHostedAgent) -> MuxaWorkGroup? {
        if let identity = participant.pane?.workIdentity,
           let group = workGroups.first(where: { $0.identity == identity }) {
            return group
        }
        return workGroups.first { group in
            group.participants.contains { $0.id == participant.id }
        }
    }
}
