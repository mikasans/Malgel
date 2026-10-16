= Typst navigation sample

Click the preview pages to jump back here. Press F12 on `@fig` or `@sec` for go-to-definition.

// Heading refs need numbering; without it Typst errors and the preview has 0 pages.
#set heading(numbering: "1.")

#figure(
  rect(width: 80%, height: 40pt, fill: luma(230)),
  caption: [A labelled figure],
) <fig>

See @fig above — F12 on that reference should land on the figure.

== Details <sec>

More body text so the preview has a second page-worthy section.

#lorem(40)

#pagebreak()

== Second page

Still more content for scrolling and click-to-source.

#lorem(30)

Back to @sec and @fig.
