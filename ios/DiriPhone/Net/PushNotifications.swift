import SwiftUI
import UIKit
import UserNotifications

/// Push plumbing: asks for permission, hands the APNs token to the model
/// (which registers it with each gateway), and answers notification actions.
///
/// Approve and Deny go through the gateway's `/answer`, which re-checks that
/// the agent is still on the prompt the notification showed and writes once
/// (the notification's id is the answer id, so iOS delivering an action
/// twice still types once). A destructive prompt's Approve requires the
/// phone unlocked (`.authenticationRequired`).
@MainActor
final class PushDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    weak var model: AppModel?

    func application(_ application: UIApplication,
                     didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.setNotificationCategories(PushContent.categories)
        #if DEBUG
        deliverSimulatedPush()
        #endif
        return true
    }

    #if DEBUG
    /// `-deliverPush <payload.apns>`: runs a payload through the same
    /// composition the service extension does and posts it two seconds
    /// later. `xcrun simctl push` delivers payloads without launching service
    /// extensions, so this is how the Simulator shows what a real push looks
    /// like, and how UI tests reach its actions.
    private func deliverSimulatedPush() {
        guard let index = CommandLine.arguments.firstIndex(of: "-deliverPush"),
              index + 1 < CommandLine.arguments.count,
              let data = FileManager.default.contents(atPath: CommandLine.arguments[index + 1]),
              let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        Task {
            let content = PushContent.content(fromPayload: payload)
            await PushContent.compose(content)
            let trigger = UNTimeIntervalNotificationTrigger(timeInterval: 2, repeats: false)
            try? await UNUserNotificationCenter.current()
                .add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: trigger))
        }
    }
    #endif

    /// Called once the app has a gateway: asking before pairing would be a
    /// question with no context.
    func requestAuthorization() {
        Task {
            let granted = (try? await UNUserNotificationCenter.current()
                .requestAuthorization(options: [.alert, .sound, .badge])) ?? false
            if granted { UIApplication.shared.registerForRemoteNotifications() }
        }
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        model?.pushToken = deviceToken.map { String(format: "%02x", $0) }.joined()
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        // No signing team or no network: in-app attention still works.
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification)
        async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        guard let reference = PushContent.Reference(userInfo: info) else { return }
        let action = response.actionIdentifier
        let answerID = response.notification.request.identifier
        await MainActor.run { [weak self] in
            guard let model = self?.model else { return }
            switch action {
            case PushContent.approveAction, PushContent.denyAction:
                guard let occurred = reference.occurredAt else { return }
                let approve = action == PushContent.approveAction
                Task {
                    do {
                        try await model.answer(fromPushTo: reference.base, session: reference.session,
                                               approve: approve, occurredAt: occurred, answerID: answerID)
                    } catch {
                        // Refused (moved on) or unreachable: say so where the
                        // person will look next, never type anything else.
                        await PushDelegate.notifyNotSent(error, title: response.notification.request.content.title)
                    }
                }
            default:
                model.selectedSessionID = reference.session
            }
        }
    }

    /// A local notification saying an answer did not go through, and why.
    static func notifyNotSent(_ error: Error, title: String) async {
        let content = UNMutableNotificationContent()
        content.title = "Answer not sent"
        content.body = "\(title): \(SessionListView.describe(error))"
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        try? await UNUserNotificationCenter.current().add(request)
    }
}
