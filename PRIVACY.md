# Privacy policy

This policy covers the Google user data that command-line tools built on this
repository's `pk-cli-drive` crate read and write when you sign in to Google
through their OAuth client. On Google's consent screen that client is named
"Piekstra CLIs".

## What the tools access

- **Google Drive files and folders**, at the scope shown on the consent screen:
  only the files the tool made (`drive.file`), or your whole Drive (`drive`).
- **Who a file or folder is shared with**, when you run a command that asks: the
  email addresses of people and groups, the domains, the roles and expiry dates
  of their access, for the item and for the folder that holds it. The crate does
  not store them; they go only to the output of that command.
- **Your Google account's email address and Drive storage quota**, to show
  which account a sign-in belongs to and how much space is free.

A tool reads, writes, moves or deletes Drive files only when you run a
command that asks it to, or when you schedule one.

## Where the data goes

The tools run on your own computer. Data moves only between your computer and
Google's servers.

- **Not sent to the developer or third parties.** Nothing goes to the
  developer, an analytics service, an advertiser or anyone else. The tools
  have no telemetry.
- **Files go where you name them.** A file read from Drive is written only to
  the place your command names: a local directory, standard output, or another
  Drive folder. Some tools keep a local cache of what they read, in your user
  account's configuration or cache directory.
- **Sign-ins stay sealed on your machine.** The OAuth client and your sign-in
  tokens are kept in each tool's vault: files on your computer, sealed with a
  key held in the macOS Keychain. They are never written in plain text,
  logged, or printed.

## Google API Services User Data Policy

Use of information received from Google APIs adheres to the
[Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including its Limited Use requirements. Google user data is not used for
advertising, is not sold, is not used to train machine-learning models, and is
not read by people except as the tool's output to you.

## Removing access

- **Sign out on one machine:** a tool's `logout` command (`drive logout` in
  most of them) removes the tokens from your computer.
- **Revoke the grant:** `logout --revoke`, or removing "Piekstra CLIs" under
  third-party apps and services in your Google Account, revokes it at Google.
  That ends every sign-in through this client, on every machine.
- **Delete everything:** remove the tool's configuration directory, its cache
  directory and its Keychain item.

## Changes and contact

Changes to this policy are made in this file, and its history is the record of
them. Questions go to the repository's issue tracker.
