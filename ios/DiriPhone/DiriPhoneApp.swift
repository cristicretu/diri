import SwiftUI

@main
struct DiriPhoneApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                if model.endpoint == nil {
                    ConnectView()
                } else {
                    SidebarView()
                }
            }
            .environment(model)
            .preferredColorScheme(.dark)
            .tint(Tokens.Ink.clay)
            .onChange(of: scenePhase) { _, phase in model.setActive(phase == .active) }
        }
    }
}
