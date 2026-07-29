# Brainstorm

This is the brainstorm for a API that aims to be the bacj-end for a big library for the user, that's why the name "Alexandria".  

## Features

- The user can view, edit metadata and delete:
  - Music (audio files)
  - Movies and series (video files)
  - HTML pages
  - Markdown and text files
  - PDF and e-book formats
  - Images
  - Browser bookmarks

What the user can edit?

- Music metadata
- Video metadata
- Markdown and text files
- Everyfile name

So I don't want any complex operation, like audio or video editing. I just want a software that can be used to organize and show those files from disk.  

How the files can be organized?

- The user can create, update, delete and organize browser bookmarks in folders
- Can do the same for every other file type
- Can create watchlists for movies, series and track watched movies and series progress

## Technologies

- Everything written in Rust, to ensure the best performance and safety
- Must provide an API to be consumed by any other system, in any other language
- Initially the idea is to build a desktop front-end using flutter, so this API must be ready for be consumed by this front-end
- Use SQLite as the a embedded database
