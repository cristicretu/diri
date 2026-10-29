import UserNotifications

/// Writes a Diri notification from the gateway's own words. The push says
/// only which session and why; the title, the question and whether it can
/// be answered from the banner come from the gateway, over the tailnet
/// (`PushContent.compose`). When the gateway cannot be reached in time the
/// generic banner stays.
final class NotificationService: UNNotificationServiceExtension {
    private var content: UNMutableNotificationContent?
    private var deliver: ((UNNotificationContent) -> Void)?
    private var work: Task<Void, Never>?

    override func didReceive(_ request: UNNotificationRequest,
                             withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void) {
        guard let content = request.content.mutableCopy() as? UNMutableNotificationContent else {
            contentHandler(request.content)
            return
        }
        self.content = content
        deliver = contentHandler
        work = Task { [weak self] in
            await PushContent.compose(content)
            self?.finish()
        }
    }

    override func serviceExtensionTimeWillExpire() {
        work?.cancel()
        finish()
    }

    private func finish() {
        guard let deliver, let content else { return }
        self.deliver = nil
        deliver(content)
    }
}
