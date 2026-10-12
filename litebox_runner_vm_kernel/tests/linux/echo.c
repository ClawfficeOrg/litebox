// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Echoes one line of standard input. See echo.json.

#include <stdio.h>

int main(void) {
    char line[256];
    if (fgets(line, sizeof(line), stdin) == NULL) {
        return 1;
    }
    fputs(line, stdout);
    return 0;
}
