CC:=gcc
CFLAGS:=-Os -static
BINARIES=$(patsubst util/%.c,build/%,$(wildcard util/*.c))

all: ${BINARIES}

build/%: util/%.c
	${CC} ${CFLAGS} $< -o $@
	strip --strip-debug --strip-unneeded $@

clean:
	rm -f build/*
