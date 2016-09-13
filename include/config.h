#ifndef PIXIE_CONFIG_H
#define PIXIE_CONFIG_H

// This value can be overridden by the PIXIE_HTTP_PORT environment variable.
#define DEFAULT_HTTP_PORT 80

// This value can be overridden by the chunk_size property in a JSON config.
#define DEFAULT_CHUNK_SIZE (1<<22)

#endif
