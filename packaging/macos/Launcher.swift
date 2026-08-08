import AppKit
import Foundation

private let installRoot = URL(fileURLWithPath: "/Applications/GnomeAI-RS.app", isDirectory: true)
private let resourcesRoot = installRoot.appendingPathComponent("Contents/Resources", isDirectory: true)
private let executableRoot = installRoot.appendingPathComponent("Contents/MacOS", isDirectory: true)
private let codexExecutable = resourcesRoot.appendingPathComponent("codex/bin/codex")
private let webURL = URL(string: "http://127.0.0.1:8787/")!

private func quotedForShell(_ value: String) -> String {
    "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
}

private func stateRoot() throws -> URL {
    let root = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/GnomeAI-RS", isDirectory: true)
    try FileManager.default.createDirectory(
        at: root,
        withIntermediateDirectories: true,
        attributes: [.posixPermissions: 0o700]
    )
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: root.path)
    return root
}

private func showFailure(_ message: String) {
    let application = NSApplication.shared
    application.setActivationPolicy(.accessory)
    application.activate(ignoringOtherApps: true)

    let alert = NSAlert()
    alert.alertStyle = .critical
    alert.messageText = "GnomeAI-RS nu a pornit"
    alert.informativeText = message
    alert.runModal()
}

private func launchAgent(executable: String) throws {
    let root = try stateRoot()
    let commandFile = root.appendingPathComponent("launch-agent.command")
    let agent = executableRoot.appendingPathComponent(executable).path
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    let script = """
    #!/bin/zsh
    export GNOMEF_RS_HOME=\(quotedForShell(root.path))
    export GNOMEF_RS_ASSETS=\(quotedForShell(resourcesRoot.path))
    export GNOMEF_CODEX_BIN=\(quotedForShell(codexExecutable.path))
    cd \(quotedForShell(home))
    exec \(quotedForShell(agent)) \(quotedForShell(home))
    """

    try Data(script.utf8).write(to: commandFile, options: .atomic)
    try FileManager.default.setAttributes(
        [.posixPermissions: 0o700],
        ofItemAtPath: commandFile.path
    )

    let terminal = Process()
    terminal.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    terminal.arguments = ["-a", "Terminal", commandFile.path]
    try terminal.run()
}

private func serverIsReady() -> Bool {
    var request = URLRequest(url: webURL)
    request.timeoutInterval = 1.0
    let semaphore = DispatchSemaphore(value: 0)
    var ready = false

    URLSession.shared.dataTask(with: request) { _, response, _ in
        if let response = response as? HTTPURLResponse {
            ready = (200..<500).contains(response.statusCode)
        }
        semaphore.signal()
    }.resume()

    _ = semaphore.wait(timeout: .now() + 2.0)
    return ready
}

private func launchWebTool() throws {
    let root = try stateRoot()
    let logRoot = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/GnomeAI-RS", isDirectory: true)
    try FileManager.default.createDirectory(
        at: logRoot,
        withIntermediateDirectories: true,
        attributes: nil
    )
    let logURL = logRoot.appendingPathComponent("WebTool.log")

    if !serverIsReady() {
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: nil)
        }
        let log = try FileHandle(forWritingTo: logURL)
        try log.seekToEnd()

        let web = Process()
        web.executableURL = executableRoot.appendingPathComponent("gnomef-web")
        web.currentDirectoryURL = FileManager.default.homeDirectoryForCurrentUser
        var environment = ProcessInfo.processInfo.environment
        environment["GNOMEF_RS_HOME"] = root.path
        environment["GNOMEF_RS_ASSETS"] = resourcesRoot.path
        environment["GNOMEF_CODEX_BIN"] = codexExecutable.path
        web.environment = environment
        web.standardInput = FileHandle.nullDevice
        web.standardOutput = log
        web.standardError = log
        try web.run()

        for _ in 0..<40 {
            if serverIsReady() {
                break
            }
            Thread.sleep(forTimeInterval: 0.25)
        }
    }

    guard serverIsReady() else {
        throw NSError(
            domain: "com.gnomeai.rs.launcher",
            code: 1,
            userInfo: [
                NSLocalizedDescriptionKey:
                    "WebTool nu răspunde la \(webURL.absoluteString). Detalii: \(logURL.path)"
            ]
        )
    }
    NSWorkspace.shared.open(webURL)
}

do {
    if Bundle.main.bundleIdentifier == "com.gnomeai.rs.web" {
        try launchWebTool()
    } else {
        let executable = Bundle.main.bundleIdentifier == "com.gnomeai.rs.agent"
            ? "gnomef-agent"
            : "gnomef-rs"
        try launchAgent(executable: executable)
    }
} catch {
    showFailure(error.localizedDescription)
}
