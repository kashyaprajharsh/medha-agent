use super::to_html;

#[test]
fn renders_the_markdown_the_tui_renders() {
    let html = to_html(
        "**Modes** and *lazy* `/ponytail`\n\n- Lite\n- Full\n\n| a | b |\n|:--|--:|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```",
    );
    assert!(html.contains("<strong>Modes</strong>"));
    assert!(html.contains("<em>lazy</em>"));
    assert!(html.contains("<code>/ponytail</code>"));
    assert!(html.contains("<ul><li>Lite</li><li>Full</li></ul>"));
    assert!(html.contains("<th style=\"text-align:left\">a</th>"));
    assert!(html.contains("<td style=\"text-align:right\">2</td>"));
    assert!(html.contains("<pre data-lang=\"rust\"><code>fn main() {}\n</code></pre>"));
}

#[test]
fn raw_html_from_the_model_is_shown_as_text() {
    let html = to_html("<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)>");
    assert!(!html.contains("<script"));
    assert!(!html.contains("<img"));
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn only_attribute_free_inline_tags_render() {
    let html = to_html(
        "| a |\n|---|\n| one<br>two |\n\nPress <kbd>Cmd</kbd>+<KBD>K</KBD>, H<sub>2</sub>O, x<sup>2</sup>. \
         <kbd onclick=\"x()\">K</kbd> <br style=\"x\"> <b>bold</b>",
    );
    assert!(html.contains("<td>one<br>two</td>"));
    assert!(html.contains("<kbd>Cmd</kbd>+<kbd>K</kbd>"));
    assert!(html.contains("H<sub>2</sub>O, x<sup>2</sup>"));
    assert!(html.contains("&lt;kbd onclick=&quot;x()&quot;&gt;"));
    assert!(html.contains("&lt;br style=&quot;x&quot;&gt;"));
    assert!(html.contains("&lt;b&gt;bold&lt;/b&gt;"));
    assert!(!html.contains("onclick=\""));
}

#[test]
fn an_html_block_reads_as_source_unless_it_is_only_line_breaks() {
    let html = to_html("<details>\n<summary>More</summary>\nHidden\n</details>\n\n<br>\n\nafter");
    assert!(html.contains(
        "<pre data-lang=\"html\"><code>&lt;details&gt;\n&lt;summary&gt;More&lt;/summary&gt;\nHidden\n&lt;/details&gt;\n</code></pre>"
    ));
    assert!(html.contains("<br><p>after</p>"));
}

#[test]
fn only_web_and_mail_links_become_links() {
    let html = to_html(
        "[ok](https://medha.dev) [mail](mailto:a@b.c) [rel](docs/x.md) [bad](javascript:alert(1)) [data](DATA:text/html,x)",
    );
    assert!(html.contains("<a href=\"https://medha.dev\">ok</a>"));
    assert!(html.contains("<a href=\"mailto:a@b.c\">mail</a>"));
    assert!(html.contains("<a href=\"docs/x.md\">rel</a>"));
    assert!(!html.contains("javascript"));
    assert!(!html.to_lowercase().contains("href=\"data"));
    assert!(html.contains("bad"));
}

#[test]
fn images_show_alt_text_and_are_never_fetched() {
    let html = to_html("![a chart](https://tracker.example/pixel.png)");
    assert!(html.contains("<span class=\"md-image\">a chart</span>"));
    assert!(!html.contains("tracker.example"));
}

#[test]
fn an_image_names_only_a_local_file_and_cannot_break_out_of_the_attribute() {
    let html = to_html("![load](out/write-load.png)");
    assert!(html.contains("<span class=\"md-image\" data-src=\"out/write-load.png\">load</span>"));
    for remote in [
        "//tracker.example/p.png",
        "javascript:alert(1)",
        "data:image/png;base64,AA",
    ] {
        assert!(!to_html(&format!("![x]({remote})")).contains("data-src"));
    }
    let html = to_html("![x](<a\" onerror=\"x.png>)");
    assert!(!html.contains("onerror=\"x"));
}

#[test]
fn code_fence_languages_cannot_break_out_of_the_attribute() {
    let html = to_html("```rust\" onclick=\"x\nfn a() {}\n```");
    assert!(html.contains("<pre data-lang=\"rust\"><code>"));
    assert!(!html.contains("onclick"));
}
