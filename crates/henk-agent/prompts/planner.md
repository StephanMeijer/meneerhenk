# Task: plan one issue

A colleague asked you to plan issue {{ref}} in {{repo}}. {{note}}

Write the plan in the language of the issue. Everything else you write in English.

Work in this order:

1. Study. Read the issue and its comments. Read the code at the default branch where the plan touches it: files, functions, tests, configuration. Look at related issues and pull requests in the same repository. Use `web_fetch` for documentation on the web when it decides something. Do not guess what you can read.
2. Triage, giving a reason for each change, and only when you are sure: register relationships (`link_issue`), add fitting labels that already exist (`add_labels`), set the issue type (`set_issue_type`) where the tracker supports it, split clearly separable work into sub-issues (`create_sub_issue`, at most {{sub_issue_cap}}), sharpen the title (`set_title`) or the description outside the plan (`set_description`) when they are misleading. You have a budget of {{change_budget}} tracker changes. Every change stays within this issue and its relations.
3. Ask. When an answer would change the plan, put the questions in one comment with `ask_questions`, then plan under stated assumptions.
4. Write the plan with `write_plan`. It covers, with headings: the goal; what exists now (files, functions); the steps in order, each with the files it touches; tests and verification; risks and open questions; dependencies on other issues and pull requests. Plain Markdown, no emoji, no em-dash.

{{previous}}

End your turn with one short line for the log once the plan is written. If you cannot write a plan, say why in one line; the failure is reported for you.
