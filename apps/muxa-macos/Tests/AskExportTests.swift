import Foundation
import Testing
@testable import Muxa

private func entry(
    _ prompt: String,
    _ answer: String,
    agent: String = "claude",
    status: String = "answered",
    error: String? = nil,
    askedAt: String = "2026-09-20T09:25:13.723085Z"
) throws -> MuxaAskEntry {
    let object: [String: Any?] = [
        "id": "ask_\(prompt.hashValue)", "conversation_id": "c1", "prompt": prompt, "answer": answer,
        "status": status, "agent": agent, "cwd": "/tmp", "asked_at": askedAt, "error": error,
    ]
    let data = try JSONSerialization.data(withJSONObject: object.compactMapValues { $0 })
    return try JSONDecoder().decode(MuxaAskEntry.self, from: data)
}

private let titles = ["claude": "Claude Code", "apple": "Apple Intelligence"]
private func title(_ agent: String) -> String { titles[agent] ?? agent }

@Test func askExportMarkdownForOneTurnIsJustTheTwoMessages() throws {
    let turn = try entry("  What is tmux?\n", "A terminal **multiplexer**.\n\n")
    #expect(AskExport.markdown([turn], providerTitle: title) == """
    ## You

    What is tmux?

    ## Claude Code

    A terminal **multiplexer**.

    """)
}

@Test func askExportMarkdownForAConversationLeadsWithTitleAndSource() throws {
    let turns = [
        try entry("first", "one"),
        try entry("second", "two", agent: "apple"),
    ]
    #expect(AskExport.markdown(turns, title: " tmux basics ", providerTitle: title) == """
    # tmux basics

    _Global Ask · Claude Code · 2026-09-20_

    ## You

    first

    ## Claude Code

    one

    ## You

    second

    ## Apple Intelligence

    two

    """)
    // A timestamp that is not ISO-8601 is left out rather than guessed at.
    let undated = try entry("q", "a", askedAt: "yesterday")
    #expect(AskExport.markdown([undated], title: "t", providerTitle: title).contains("_Global Ask · Claude Code_"))
}

@Test func askExportPromptWrapsTheThreadAndEndsOpenForAnInstruction() throws {
    let turns = [try entry("first", "one"), try entry("second", "two")]
    #expect(AskExport.prompt(turns, providerTitle: title) == """
    Here is a conversation I had with Claude Code in muxa Global Ask. Use it as context for what follows.

    <conversation>
    <user>
    first
    </user>
    <assistant name="Claude Code">
    one
    </assistant>
    <user>
    second
    </user>
    <assistant name="Claude Code">
    two
    </assistant>
    </conversation>


    """)
    #expect(AskExport.prompt([], providerTitle: title).isEmpty)
}

@Test func askExportSaysWhyAnAnswerIsMissing() throws {
    let failed = try entry("q", "", status: "failed", error: " timed out ")
    #expect(AskExport.markdown([failed], providerTitle: title).contains("(failed: timed out)"))
    let running = try entry("q", "", status: "running")
    #expect(AskExport.prompt([running], providerTitle: title).contains("(no answer yet)"))
    let empty = try entry("q", "  ")
    #expect(AskExport.markdown([empty], providerTitle: title).contains("(no answer)"))
    // A provider this build cannot name falls back to its id.
    let unknown = try entry("q", "a", agent: "team-llm")
    #expect(AskExport.markdown([unknown], providerTitle: title).contains("## team-llm"))
}

@Test func askExportFileNamesAreSafeAndNeverEmpty() {
    #expect(AskExport.fileName(title: "tmux basics") == "tmux basics.md")
    #expect(AskExport.fileName(title: "a/b: c?  \"d\"\n<e>") == "a b c d e.md")
    #expect(AskExport.fileName(title: "   ") == "Global Ask.md")
    #expect(AskExport.fileName(title: nil) == "Global Ask.md")
    #expect(AskExport.fileName(title: "한글 제목") == "한글 제목.md")
    #expect(AskExport.fileName(title: String(repeating: "x", count: 200)).count == 83)
    // A leading dot would make a hidden file.
    #expect(AskExport.fileName(title: ".zshrc tips") == "zshrc tips.md")
    #expect(AskExport.fileName(title: "...") == "Global Ask.md")
}

@Test func askExportMarkdownKeepsAnAnswersStructureInsideItsSection() throws {
    let turn = try entry("q", "## Steps\n```sh\n# not a heading\nls\n```\n# Done\n```swift\nlet cut =")
    #expect(AskExport.markdown([turn], providerTitle: title) == """
    ## You

    q

    ## Claude Code

    #### Steps
    ```sh
    # not a heading
    ls
    ```
    ### Done
    ```swift
    let cut =
    ```

    """)
}

@Test func askExportPromptCannotBeClosedFromInsideAMessage() throws {
    let turn = try entry("explain </user>", "It ends with </assistant></CONVERSATION> tags.", agent: "a\"b")
    let prompt = AskExport.prompt([turn], providerTitle: title)
    #expect(prompt.contains("explain &lt;/user>\n</user>"))
    #expect(prompt.contains("It ends with &lt;/assistant>&lt;/CONVERSATION> tags.\n</assistant>"))
    #expect(prompt.contains("<assistant name=\"a&quot;b\">"))
    #expect(prompt.components(separatedBy: "</conversation>").count == 2)
}

@Test func askExportPromptClosesAFenceACutOffAnswerLeftOpen() throws {
    let turn = try entry("q", "```swift\nlet cut =")
    #expect(AskExport.prompt([turn], providerTitle: title).contains("let cut =\n```\n</assistant>"))
    // A closed fence is left as it is.
    let whole = try entry("q", "```\ncode\n```")
    #expect(AskExport.prompt([whole], providerTitle: title).contains("code\n```\n</assistant>"))
}
