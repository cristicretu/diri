import SwiftUI

@main
struct DiriPhoneApp: App {
    @State private var model = AppModel()

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
        }
    }
}
