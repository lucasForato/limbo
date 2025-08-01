#!/usr/bin/env python3
#
# Copyright 2024 the Limbo authors. All rights reserved. MIT license.
#
# A script to merge a pull requests with a nice merge commit.
#
# Requirements:
#
# ```
# pip install PyGithub
# ```
import json
import os
import re
import subprocess
import sys
import tempfile
import textwrap

from github import Github


def run_command(command):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, shell=True)
    output, error = process.communicate()
    return output.decode("utf-8").strip(), error.decode("utf-8").strip(), process.returncode


def load_user_mapping(file_path=".github.json"):
    if os.path.exists(file_path):
        with open(file_path, "r") as f:
            return json.load(f)
    return {}


user_mapping = load_user_mapping()


def get_user_email(g, username):
    if username in user_mapping:
        return f"{user_mapping[username]['name']} <{user_mapping[username]['email']}>"

    try:
        user = g.get_user(username)
        name = user.name if user.name else username
        if user.email:
            return f"{name} <{user.email}>"
        return f"{name} (@{username})"
    except Exception as e:
        print(f"Error fetching email for user {username}: {str(e)}")

    # If we couldn't find an email, return a noreply address
    return f"{username} <{username}@users.noreply.github.com>"


def get_pr_info(g, repo, pr_number):
    pr = repo.get_pull(int(pr_number))
    author = pr.user
    author_name = author.name if author.name else author.login

    # Get the list of users who reviewed the PR
    reviewed_by = []
    reviews = pr.get_reviews()
    for review in reviews:
        if review.state == "APPROVED":
            reviewer = review.user
            reviewed_by.append(get_user_email(g, reviewer.login))

    return {
        "number": pr.number,
        "title": pr.title,
        "author": author_name,
        "head": pr.head.ref,
        "head_sha": pr.head.sha,
        "body": pr.body.strip() if pr.body else "",
        "reviewed_by": reviewed_by,
    }


def wrap_text(text, width=72):
    lines = text.split("\n")
    wrapped_lines = []
    in_code_block = False
    for line in lines:
        if line.strip().startswith("```"):
            in_code_block = not in_code_block
            wrapped_lines.append(line)
        elif in_code_block:
            wrapped_lines.append(line)
        else:
            wrapped_lines.extend(textwrap.wrap(line, width=width))
    return "\n".join(wrapped_lines)


def merge_pr(pr_number, use_api=True):
    # GitHub authentication
    token = os.getenv("GITHUB_TOKEN")
    g = Github(token)

    # Get the repository
    repo_name = os.getenv("GITHUB_REPOSITORY")
    if not repo_name:
        print("Error: GITHUB_REPOSITORY environment variable not set")
        sys.exit(1)
    repo = g.get_repo(repo_name)

    # Get PR information
    pr_info = get_pr_info(g, repo, pr_number)

    # Format commit message
    commit_title = f"Merge '{pr_info['title']}' from {pr_info['author']}"
    commit_body = wrap_text(pr_info["body"])

    commit_message = f"{commit_title}\n\n{commit_body}\n"

    # Add Reviewed-by lines
    for approver in pr_info["reviewed_by"]:
        commit_message += f"\nReviewed-by: {approver}"

    # Add Closes line
    commit_message += f"\n\nCloses #{pr_info['number']}"

    if use_api:
        # Merge using GitHub API
        try:
            pr = pr_info["pr_object"]
            # Check if PR is mergeable
            if not pr.mergeable:
                print(f"Error: PR #{pr_number} is not mergeable. State: {pr.mergeable_state}")
                sys.exit(1)
            result = pr.merge(
                commit_title=commit_title,
                commit_message=commit_message.replace(commit_title + "\n\n", ""),
                merge_method="merge",
            )
            if result.merged:
                print(f"Pull request #{pr_number} merged successfully via GitHub API!")
                print(f"Merge commit SHA: {result.sha}")
                print(f"\nMerge commit message:\n{commit_message}")
            else:
                print(f"Error: Failed to merge PR #{pr_number}")
                print(f"Message: {result.message}")
                sys.exit(1)
        except Exception as e:
            print(f"Error merging PR via API: {str(e)}")
            sys.exit(1)

    else:
        # Create a temporary file for the commit message
        with tempfile.NamedTemporaryFile(mode="w", delete=False) as temp_file:
            temp_file.write(commit_message)
            temp_file_path = temp_file.name

        try:
            # Instead of fetching to a branch, fetch the specific commit
            cmd = f"git fetch origin pull/{pr_number}/head"
            output, error, returncode = run_command(cmd)
            if returncode != 0:
                print(f"Error fetching PR: {error}")
                sys.exit(1)

            # Checkout main branch
            cmd = "git checkout main"
            output, error, returncode = run_command(cmd)
            if returncode != 0:
                print(f"Error checking out main branch: {error}")
                sys.exit(1)

            # Merge using the commit SHA instead of branch name
            cmd = f"git merge --no-ff {pr_info['head_sha']} -F {temp_file_path}"
            output, error, returncode = run_command(cmd)
            if returncode != 0:
                print(f"Error merging PR: {error}")
                sys.exit(1)

            print("Pull request merged successfully!")
            print(f"Merge commit message:\n{commit_message}")
            print("\nNote: You'll need to push this merge to mark the PR as merged on GitHub")

        finally:
            # Clean up the temporary file
            os.unlink(temp_file_path)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("Usage: python merge_pr.py <pr_number>")
        sys.exit(1)

    pr_number = sys.argv[1]
    if not re.match(r"^\d+$", pr_number):
        print("Error: PR number must be a positive integer")
        sys.exit(1)

    use_api = True
    if len(sys.argv) == 3 and sys.argv[2] == "--local":
        use_api = False

    merge_pr(pr_number, use_api)
