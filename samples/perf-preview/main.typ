= Perf preview fixture

A small Typst doc with one raster image for cold/warm RSS measurement.

#set heading(numbering: "1.")

#figure(
  image("photo.png", width: 80%),
  caption: [Embedded photo for image-load path],
) <photo>

See @photo. Body text keeps layout non-trivial.

#lorem(60)

#pagebreak()

== Second page

Warm compile should reuse the sheet hash for unchanged pages when only this text edits.

#lorem(40)
