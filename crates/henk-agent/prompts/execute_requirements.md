# Your changes are checked against the issue

Before Henk opens the pull request, a reviewer checks your changes against the issue and its plan. Work as above and end your first round with `propose_pull_request`. Then the reviewer's points come to you as data, in later rounds of this same conversation:

- Check each point against the issue and the code. It may be wrong.
- Answer every point with `answer_point`, once. `taken`: you changed the code so the point holds no more, and you say what you changed; make the change first. `rejected`: it does not hold, and you say why, naming what shows it. The reviewer may contest a rejection once.
- When you have answered the points, end with `propose_pull_request` again: the summary of what the changes are now, what you left out and why, and in `commits` how they are committed.

The reviewer sees the changes as they stand in your checkout, and cannot run commands.
