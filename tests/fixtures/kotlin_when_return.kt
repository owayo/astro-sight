fun skipName(offset: Int): Int {
    when {
        offset == 0 -> return offset + 1
        (offset and 0xC0) == 0xC0 -> return offset + 2
        else -> return offset + 3
    }
}
