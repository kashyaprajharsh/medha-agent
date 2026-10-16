import AppKit
import WebKit

// A hidden WebKit instance, matching the desktop's browser engine. No user's
// app, settings, credentials or model service is opened by this check.
final class Probe: NSObject, WKNavigationDelegate {
    let web = WKWebView(frame: NSRect(x: 0, y: 0, width: 1400, height: 1000))
    var darkIDs = Set<String>()
    var evaluating = false
    var timer: Timer?
    let started = Date()
    var trustFixture = false
    var darkBackground: String?

    func start(_ url: URL) {
        trustFixture = url.lastPathComponent == "trust.html"
        web.navigationDelegate = self
        web.load(URLRequest(url: url))
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [self] _ in
            guard !evaluating else { return }
            evaluating = true
            web.evaluateJavaScript("JSON.stringify(window.fixtureReport?.())") { [self] value, error in
                evaluating = false
                if trustFixture {
                    checkTrust(value, error)
                    return
                }
                if let text = value as? String, let data = text.data(using: .utf8),
                   let report = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                   let checks = report["checks"] as? [[String: Any]], checks.count == 8,
                   let viewer = report["viewer"] as? [String: Any] {
                    let surfaces = checks.flatMap { [$0["plate"], $0["thumbnail"]].compactMap { $0 as? [String: Any] } } + [viewer]
                    let ids = Set(surfaces.compactMap { $0["id"] as? String })
                    // Wait for every plate, thumbnail and viewer to render, and
                    // for every old SVG to be replaced after the theme change.
                    if report["fontsLoaded"] as? Bool == true, surfaces.count == 17, ids.count == 17,
                       surfaces.allSatisfy({ ($0["labels"] as? [Any] ?? []).count >= 4 }),
                       darkIDs.isEmpty || ids.isDisjoint(with: darkIDs) {
                        let incident = checks.last!
                        let expected = report["incidentLabels"] as? [String] ?? []
                        let incidentSurfaces = [incident["plate"], incident["thumbnail"], viewer].compactMap { $0 as? [String: Any] }
                        let incidentComplete = incidentSurfaces.count == 3 && incidentSurfaces.allSatisfy { surface in
                            let labels = surface["labels"] as? [[String: Any]] ?? []
                            let words = labels.compactMap { $0["text"] as? String }
                            return !expected.isEmpty && expected.allSatisfy { wanted in words.contains { $0.contains(wanted) } }
                        }
                        let valid = incidentComplete && (report["errors"] as? [Any] ?? []).isEmpty && surfaces.allSatisfy { surface in
                            (surface["foreign"] as? Int) == 0 && (surface["labels"] as? [[String: Any]] ?? []).allSatisfy { label in
                                (label["width"] as? Double ?? 0) > 0 && (label["height"] as? Double ?? 0) > 0 &&
                                (label["screenWidth"] as? Double ?? 0) > 0 && (label["screenHeight"] as? Double ?? 0) > 0 &&
                                label["visible"] as? Bool == true &&
                                (label["fill"] as? String) != "none" && (label["opacity"] as? String) != "0"
                            }
                        }
                        if !valid { print("Invalid artifact labels: " + text); exit(2) }
                        if darkIDs.isEmpty, report["theme"] as? String == "dark" {
                            print("WebKit dark: 8 diagrams, 17 production surfaces retain SVG labels.")
                            darkIDs = ids
                            web.evaluateJavaScript("document.querySelector('#theme').click()", completionHandler: nil)
                        } else if !darkIDs.isEmpty, report["theme"] as? String == "light" {
                            print("WebKit light: all 17 surfaces redrawn; all reported diagram labels remain visible.")
                            timer?.invalidate()
                            web.evaluateJavaScript("document.documentElement.dataset.hideLabels = ''; JSON.stringify(window.fixtureReport())") { value, error in
                                guard let text = value as? String, let data = text.data(using: .utf8), error == nil,
                                      let hidden = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                                      let checks = hidden["checks"] as? [[String: Any]], checks.count == 8 else { exit(6) }
                                let caught = checks.allSatisfy { check in
                                    ["plate", "thumbnail"].allSatisfy { kind in
                                        let labels = (check[kind] as? [String: Any])?["labels"] as? [[String: Any]] ?? []
                                        return !labels.isEmpty && labels.allSatisfy { $0["visible"] as? Bool == false }
                                    }
                                }
                                if !caught { print("Visibility check accepted hidden labels"); exit(7) }
                                print("WebKit negative control: labels hidden by an ancestor are rejected despite nonzero text bounds.")
                                exit(0)
                            }
                        }
                    }
                }
                if Date().timeIntervalSince(started) > 60 {
                    print("Artifact render timeout: \(value ?? "none"); \(String(describing: error))")
                    exit(3)
                }
            }
        }
    }
    func checkTrust(_ value: Any?, _ error: Error?) {
        guard let text = value as? String, let data = text.data(using: .utf8), error == nil,
              let report = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              report["fontsLoaded"] as? Bool == true,
              let cards = report["cards"] as? [[String: Any]], cards.count == 2 else {
            if Date().timeIntervalSince(started) > 60 { print("Trust fixture timeout: \(value ?? "none")"); exit(8) }
            return
        }
        guard report["dialogs"] as? Int == 0, cards.allSatisfy({ card in
            (card["width"] as? Double ?? 0) >= 650 && (card["width"] as? Double ?? 0) <= 704 &&
            (card["height"] as? Double ?? 0) > 100 && card["font"] as? String == "15px" &&
            card["buttons"] as? Int == 2 && card["overflow"] as? Bool == false
        }) else { print("Invalid trust notice: " + text); exit(9) }
        let theme = report["theme"] as? String ?? "dark"
        let background = cards[0]["background"] as? String ?? ""
        if let previous = darkBackground, previous == background { print("Trust theme did not change"); exit(10) }
        evaluating = true
        let finish = { [self] in
            print("WebKit \(theme): two production trust notices at 704 CSS pixels, 15px title, no overflow or dialogs.")
            if darkBackground == nil {
                darkBackground = background
                web.evaluateJavaScript("document.documentElement.dataset.theme = 'light'") { _, _ in self.evaluating = false }
            } else { timer?.invalidate(); exit(0) }
        }
        guard CommandLine.arguments.count == 3 else { finish(); return }
        let directory = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        do { try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true) }
        catch { print("Cannot create screenshot directory: \(error)"); exit(11) }
        saveTrustCard(cards, index: 0, theme: theme, directory: directory, finish: finish)
    }

    func saveTrustCard(_ cards: [[String: Any]], index: Int, theme: String, directory: URL, finish: @escaping () -> Void) {
        guard index < cards.count else { finish(); return }
        let card = cards[index]
        let config = WKSnapshotConfiguration()
        config.rect = CGRect(x: card["x"] as? Double ?? 0, y: card["y"] as? Double ?? 0,
                             width: card["width"] as? Double ?? 0, height: card["height"] as? Double ?? 0)
        config.snapshotWidth = NSNumber(value: (card["width"] as? Double ?? 0) / (NSScreen.main?.backingScaleFactor ?? 1))
        web.takeSnapshot(with: config) { [self] image, error in
            guard error == nil, let tiff = image?.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff),
                  let bytes = bitmap.representation(using: .png, properties: [:]), let state = card["state"] as? String else {
                print("Cannot capture trust notice: \(String(describing: error))"); exit(12)
            }
            do { try bytes.write(to: directory.appendingPathComponent("\(theme)-\(state).png")) }
            catch { print("Cannot save trust notice: \(error)"); exit(13) }
            saveTrustCard(cards, index: index + 1, theme: theme, directory: directory, finish: finish)
        }
    }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        print("Artifact fixture navigation failed: \(error)"); exit(4)
    }
}
guard (2...3).contains(CommandLine.arguments.count), let url = URL(string: CommandLine.arguments[1]) else { exit(5) }
NSApplication.shared.setActivationPolicy(.prohibited)
let probe = Probe()
probe.start(url)
NSApplication.shared.run()
