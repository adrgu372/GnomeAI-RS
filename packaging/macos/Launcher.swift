import AppKit
import Foundation

private let installRoot = URL(fileURLWithPath: "/Applications/GnomeAI-RS.app", isDirectory: true)
private let resourcesRoot = installRoot.appendingPathComponent("Contents/Resources", isDirectory: true)
private let executableRoot = installRoot.appendingPathComponent("Contents/MacOS", isDirectory: true)
private let codexExecutable = resourcesRoot.appendingPathComponent("codex/bin/codex")

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
    alert.messageText = "GnomeAI-RS could not start"
    alert.informativeText = message
    alert.runModal()
}

private func launchAgent(executable: String) throws {
    let root = try stateRoot()
    let process = Process()
    process.executableURL = executableRoot.appendingPathComponent(executable)
    process.currentDirectoryURL = FileManager.default.homeDirectoryForCurrentUser
    var environment = ProcessInfo.processInfo.environment
    environment["GNOMEF_RS_HOME"] = root.path
    environment["GNOMEF_RS_ASSETS"] = resourcesRoot.path
    environment["GNOMEF_CODEX_BIN"] = codexExecutable.path
    process.environment = environment
    try process.run()
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        throw NSError(
            domain: "com.gnomeai.rs.launcher",
            code: Int(process.terminationStatus),
            userInfo: [
                NSLocalizedDescriptionKey:
                    "The application exited with code \(process.terminationStatus)."
            ]
        )
    }
}

do {
    try launchAgent(executable: "gnomef-rs")
} catch {
    showFailure(error.localizedDescription)
}
