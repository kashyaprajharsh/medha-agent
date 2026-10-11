import AppKit
import WebKit

final class Scheme: NSObject, WKURLSchemeHandler {
    let fixture: [String: Any]
    init(_ fixture: [String: Any]) { self.fixture = fixture }
    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        let url = task.request.url!
        let pages = fixture["pages"] as! [String]
        let replies = fixture["replies"] as! [String: [String: Any]]
        if url.path == "/fixture" {
            let html = "<!doctype html><script>window.blocked=[];addEventListener('message',e=>{if(e.data?.kind==='medha-preview-blocked')window.blocked.push(e.data)})</script>" + pages.enumerated().map { "<iframe id='frame\($0.offset)' sandbox='allow-scripts' src='\($0.element)'></iframe>" }.joined()
            task.didReceive(HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/html"])!)
            task.didReceive(html.data(using: .utf8)!)
        } else if let reply = replies[url.path], let body = Data(base64Encoded: reply["body"] as! String) {
            task.didReceive(HTTPURLResponse(url: url, statusCode: reply["status"] as! Int, httpVersion: "HTTP/1.1", headerFields: reply["headers"] as? [String: String])!)
            task.didReceive(body)
        } else {
            task.didReceive(HTTPURLResponse(url: url, statusCode: 404, httpVersion: "HTTP/1.1", headerFields: nil)!)
            task.didReceive(Data())
        }
        task.didFinish()
    }
    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}
}

final class Probe: NSObject, WKScriptMessageHandler, WKNavigationDelegate {
    let web: WKWebView
    let pages: [String]
    var reports: [String: [String: Any]] = [:]
    var blocked: [[String: Any]] = []
    let started = Date()
    var timer: Timer?
    init(_ fixture: [String: Any]) {
        pages = fixture["pages"] as! [String]
        let configuration = WKWebViewConfiguration()
        configuration.setURLSchemeHandler(Scheme(fixture), forURLScheme: "medha-view")
        web = WKWebView(frame: NSRect(x: 0, y: 0, width: 800, height: 600), configuration: configuration)
        super.init()
        let source = """
        setInterval(()=>{
          if(location.pathname==='/fixture'){window.webkit.messageHandlers.probe.postMessage({parent:true,blocked:window.blocked??[]});return;}
          const box=document.querySelector('#box'),image=document.querySelector('#logo');if(!box)return;
          const style=getComputedStyle(box);
          window.webkit.messageHandlers.probe.postMessage({url:location.href,background:style.backgroundColor,width:style.width,border:style.borderTopWidth,image:image?.complete&&image?.naturalWidth===16,author:window.authorRan===true,module:window.moduleRan??0});
        },100);
        """
        configuration.userContentController.add(self, name: "probe")
        configuration.userContentController.addUserScript(WKUserScript(source: source, injectionTime: .atDocumentEnd, forMainFrameOnly: false))
        web.navigationDelegate = self
    }
    func start() {
        web.load(URLRequest(url: URL(string: "medha-view://localhost/fixture")!))
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [self] _ in
            if pages.allSatisfy({ reports[$0] != nil }) {
                let still = reports[pages[0]]!, running = reports[pages[1]]!
                let assets = [still, running].allSatisfy { $0["background"] as? String == "rgb(1, 2, 3)" && $0["width"] as? String == "321px" && $0["border"] as? String == "9px" && $0["image"] as? Bool == true }
                let scripts = still["author"] as? Bool == false && still["module"] as? Int == 0 && running["author"] as? Bool == true && running["module"] as? Int == 32
                let ids = Set(pages.map { URL(string: $0)!.path.components(separatedBy: "/")[2] })
                let feedback = Set(blocked.filter { ($0["uri"] as? String ?? "").contains("blocked.invalid") }.compactMap { $0["id"] as? String })
                if assets && scripts && ids.isSubset(of: feedback) {
                    print("WebKit: relative CSS imports, SVG, JS modules, static script denial and per-frame blocked-resource feedback passed.")
                    exit(0)
                }
            }
            if Date().timeIntervalSince(started) > 45 {
                print("WebKit preview mismatch: \(reports); blocked: \(blocked)")
                exit(2)
            }
        }
    }
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let report = message.body as? [String: Any] else { return }
        if report["parent"] as? Bool == true { blocked = report["blocked"] as? [[String: Any]] ?? [] }
        else if let url = report["url"] as? String { reports[url] = report }
    }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { print(error); exit(3) }
}
guard CommandLine.arguments.count == 2,
      let data = try? Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])),
      let fixture = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { exit(4) }
NSApplication.shared.setActivationPolicy(.prohibited)
let probe = Probe(fixture)
probe.start()
NSApplication.shared.run()
