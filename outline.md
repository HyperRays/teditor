# Outline

This text editor kind of tries to be like a terminal text editor, but with first class support for mouse input. For this reason, we implement the following feattures:

- Custom high quality font rendering with wgpu
- Custom terminal like cell's indexing
- Text framebuffer
- On the fly generateable bitmaps to be displayed alligned to the terminal
- marginless tiled pane system
- virtual terminal window, which tries to minimize flickering (meaning we have a batched renderer, whose only display output is the final frame displayed)

### General UI layout

Like with most text editors, we will have dynamically resizeable windows (quantized to the cell sizes, so the encompassing window size can only have some certain fixed height and widths.)

The editor uses marginless tiled panes that fill the entire application window. A full-height folder pane sits on the left; the right workspace contains the text editor, with its open tabs in a fixed row underneath. Cmd+P opens a file/command palette in the bottommost row of the application. Panes share draggable dividers and resize in whole cells. They do not float, overlap, or move independently; there are no exterior gaps, shadows, or close/collapse controls.

### Composition

This project should be composed of a couple of libraries:
- Main backend renderer for text
- Terminal like interface with framebuffers and mouse interactions
- Ui library for tiled panes, shared dividers, buttons, etc.
- Actual editor implementation: filesystem tree, disk-backed document buffers without an application size cap, background loading/saving, format and encoding detection with manual conversion, cached syntect syntax highlighting, multiline editing, mouse selection, tabs, save/undo, and a bottom-row command palette.

