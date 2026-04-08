package com.example;

import java.util.List;
import java.util.ArrayList;

public class SampleService {
    private final List<String> items;

    public SampleService() {
        this.items = new ArrayList<>();
    }

    public void addItem(String item) {
        if (item != null) {
            items.add(item);
        }
    }

    public List<String> getItems() {
        return items;
    }

    public int getItemCount() {
        return items.size();
    }
}
