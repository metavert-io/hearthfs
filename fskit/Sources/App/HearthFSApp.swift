import SwiftUI

@main
struct HearthFSApp: App {
    var body: some Scene {
        WindowGroup {
            OnboardingView()
        }
        .windowResizability(.contentSize)
    }
}
